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
//! **Network Policy**: No network calls occur without explicit user consent.
//! The download system is consent-gated via [`ModelState::NeedsConsent`].

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

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

/// A file in the model manifest.
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

    /// Google EmbeddingGemma 300M manifest.
    ///
    /// Released: 2025-12-15
    /// Dimension: 256
    /// Best-in-class performance for its size on MTEB.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn embeddinggemma() -> Self {
        Self {
            id: "embeddinggemma-300m".into(),
            repo: "google/embeddinggemma-300m".into(),
            revision: "main".into(), // TODO: Pin to specific revision after verification
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 600_000_000, // ~600MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// Qwen3-Embedding 0.6B manifest.
    ///
    /// Released: 2025-11-20
    /// Dimension: 1024
    /// High-quality embeddings with Qwen3 architecture.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn qwen3_embedding() -> Self {
        Self {
            id: "Qwen3-Embedding-0.6B".into(),
            repo: "Alibaba-NLP/Qwen3-Embedding-0.6B".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1_200_000_000, // ~1.2GB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 2_000_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// ModernBERT-embed-large manifest.
    ///
    /// Released: 2025-12-01
    /// Dimension: 768
    /// Modern BERT variant with rotary position embeddings.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn modernbert_embed() -> Self {
        Self {
            id: "ModernBERT-embed-large".into(),
            repo: "lightonai/ModernBERT-embed-large".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 400_000_000, // ~400MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "MIT".into(),
        }
    }

    /// Snowflake Arctic Embed S manifest.
    ///
    /// Released: 2025-11-10
    /// Dimension: 384
    /// Small, fast model with MiniLM-compatible dimension.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn snowflake_arctic_s() -> Self {
        Self {
            id: "snowflake-arctic-embed-s".into(),
            repo: "Snowflake/snowflake-arctic-embed-s".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 110_000_000, // ~110MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
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
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn nomic_embed() -> Self {
        Self {
            id: "nomic-embed-text-v1.5".into(),
            repo: "nomic-ai/nomic-embed-text-v1.5".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 280_000_000, // ~280MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    // ==================== Reranker Models ====================

    /// MS MARCO MiniLM reranker manifest (baseline for bake-off).
    pub fn msmarco_reranker() -> Self {
        Self {
            id: "ms-marco-MiniLM-L-6-v2".into(),
            repo: "cross-encoder/ms-marco-MiniLM-L-6-v2".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "onnx/model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 90_000_000,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// BGE Reranker v2 M3 manifest.
    ///
    /// Released: 2025-11-15
    /// Updated BGE model with improved quality.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn bge_reranker_v2() -> Self {
        Self {
            id: "bge-reranker-v2-m3".into(),
            repo: "BAAI/bge-reranker-v2-m3".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 560_000_000, // ~560MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "MIT".into(),
        }
    }

    /// Jina Reranker v1 Turbo EN manifest.
    ///
    /// Released: 2025-11-20
    /// Fast, optimized for English.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn jina_reranker_turbo() -> Self {
        Self {
            id: "jina-reranker-v1-turbo-en".into(),
            repo: "jinaai/jina-reranker-v1-turbo-en".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 140_000_000, // ~140MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// Jina Reranker v2 Base Multilingual manifest.
    ///
    /// Released: 2025-12-01
    /// Multilingual support.
    ///
    /// Note: Checksums are placeholders until model is downloaded and verified.
    pub fn jina_reranker_v2() -> Self {
        Self {
            id: "jina-reranker-v2-base-multilingual".into(),
            repo: "jinaai/jina-reranker-v2-base-multilingual".into(),
            revision: "main".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 280_000_000, // ~280MB estimated
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500_000,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 1000,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD".into(),
                    size: 500,
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
            "embeddinggemma" => Some(Self::embeddinggemma()),
            "qwen3-embed" => Some(Self::qwen3_embedding()),
            "modernbert-embed" => Some(Self::modernbert_embed()),
            "snowflake-arctic-s" => Some(Self::snowflake_arctic_s()),
            "nomic-embed" => Some(Self::nomic_embed()),
            _ => None,
        }
    }

    /// Get manifest by reranker name.
    pub fn for_reranker(name: &str) -> Option<Self> {
        match name {
            "ms-marco" => Some(Self::msmarco_reranker()),
            "bge-reranker-v2" => Some(Self::bge_reranker_v2()),
            "jina-reranker-turbo" => Some(Self::jina_reranker_turbo()),
            "jina-reranker-v2" => Some(Self::jina_reranker_v2()),
            _ => None,
        }
    }

    /// Get all bake-off eligible embedder manifests.
    pub fn bakeoff_embedder_candidates() -> Vec<Self> {
        vec![
            Self::embeddinggemma(),
            Self::qwen3_embedding(),
            Self::modernbert_embed(),
            Self::snowflake_arctic_s(),
            Self::nomic_embed(),
        ]
    }

    /// Get all bake-off eligible reranker manifests.
    pub fn bakeoff_reranker_candidates() -> Vec<Self> {
        vec![
            Self::bge_reranker_v2(),
            Self::jina_reranker_turbo(),
            Self::jina_reranker_v2(),
        ]
    }

    /// Get all bake-off eligible model manifests (embedders + rerankers).
    pub fn bakeoff_candidates() -> Vec<Self> {
        let mut candidates = Self::bakeoff_embedder_candidates();
        candidates.extend(Self::bakeoff_reranker_candidates());
        candidates
    }

    /// Get only production-ready bake-off candidates.
    ///
    /// These are models that have verified checksums and pinned revisions,
    /// and can be safely downloaded and used.
    pub fn bakeoff_candidates_ready() -> Vec<Self> {
        Self::bakeoff_candidates()
            .into_iter()
            .filter(|m| m.is_production_ready())
            .collect()
    }

    /// Get bake-off candidates that are NOT production-ready.
    ///
    /// These models have placeholder checksums or unpinned revisions and
    /// cannot be downloaded until they are properly verified.
    pub fn bakeoff_candidates_pending() -> Vec<Self> {
        Self::bakeoff_candidates()
            .into_iter()
            .filter(|m| !m.is_production_ready())
            .collect()
    }

    /// Total size of all files in bytes.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// HuggingFace download URL for a file.
    pub fn download_url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, file.name
        )
    }
}

/// Progress callback for downloads.
pub type ProgressCallback = Box<dyn Fn(DownloadProgress) + Send + Sync>;

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
#[derive(Debug)]
pub enum DownloadError {
    /// Network error during download.
    NetworkError(String),
    /// File I/O error.
    IoError(std::io::Error),
    /// SHA256 verification failed.
    VerificationFailed {
        file: String,
        expected: String,
        actual: String,
    },
    /// Download was cancelled.
    Cancelled,
    /// Timeout during download.
    Timeout,
    /// HTTP error response.
    HttpError { status: u16, message: String },
    /// Manifest has placeholder checksums and is not production-ready.
    ///
    /// This error is returned when attempting to download a bake-off candidate
    /// model that has not yet been verified. The model files need to be:
    /// 1. Downloaded manually to compute SHA256 checksums
    /// 2. Revision pinned to a specific commit (not "main")
    ManifestNotVerified {
        model_id: String,
        unverified_files: Vec<String>,
        revision_unpinned: bool,
    },
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::NetworkError(msg) => write!(f, "network error: {msg}"),
            DownloadError::IoError(err) => write!(f, "I/O error: {err}"),
            DownloadError::VerificationFailed {
                file,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "verification failed for {file}: expected {expected}, got {actual}"
                )
            }
            DownloadError::Cancelled => write!(f, "download cancelled"),
            DownloadError::Timeout => write!(f, "download timed out"),
            DownloadError::HttpError { status, message } => {
                write!(f, "HTTP error {status}: {message}")
            }
            DownloadError::ManifestNotVerified {
                model_id,
                unverified_files,
                revision_unpinned,
            } => {
                write!(
                    f,
                    "model '{}' is not production-ready: {} file(s) have placeholder checksums{}",
                    model_id,
                    unverified_files.len(),
                    if *revision_unpinned {
                        " and revision is not pinned"
                    } else {
                        ""
                    }
                )
            }
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DownloadError::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DownloadError {
    fn from(err: std::io::Error) -> Self {
        DownloadError::IoError(err)
    }
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

        // Create temp directory
        fs::create_dir_all(&self.temp_dir)?;

        let grand_total = manifest.total_size();
        let total_files = manifest.files.len();
        let bytes_downloaded = Arc::new(AtomicU64::new(0));

        for (idx, file) in manifest.files.iter().enumerate() {
            if self.is_cancelled() {
                self.cleanup_temp();
                return Err(DownloadError::Cancelled);
            }

            // Use local_name() for local path (handles onnx/model.onnx -> model.onnx)
            let file_path = self.temp_dir.join(file.local_name());
            let url = manifest.download_url(file);

            // Track bytes_downloaded at start of this file to reset on retry
            let bytes_before_file = bytes_downloaded.load(Ordering::SeqCst);

            // Download with retries
            let mut last_error = None;
            for attempt in 0..self.max_retries {
                if self.is_cancelled() {
                    self.cleanup_temp();
                    return Err(DownloadError::Cancelled);
                }

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
                    Err(e) => {
                        last_error = Some(e);
                    }
                }
            }

            if let Some(err) = last_error {
                self.cleanup_temp();
                return Err(err);
            }

            // Verify SHA256
            if self.is_cancelled() {
                self.cleanup_temp();
                return Err(DownloadError::Cancelled);
            }

            let actual_hash = compute_sha256(&file_path)?;
            if actual_hash != file.sha256 {
                self.cleanup_temp();
                return Err(DownloadError::VerificationFailed {
                    file: file.name.clone(),
                    expected: file.sha256.clone(),
                    actual: actual_hash,
                });
            }
        }

        // Atomic install: rename temp -> target
        self.atomic_install()?;

        // Write verified marker
        self.write_verified_marker(manifest)?;

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

        // Build request with Range header for resume
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(self.connect_timeout)
            .timeout(self.file_timeout)
            .build()
            .map_err(|e| DownloadError::NetworkError(e.to_string()))?;

        let mut request = client.get(url);

        // Resume from existing size
        if existing_size > 0 {
            request = request.header("Range", format!("bytes={}-", existing_size));
            bytes_downloaded.fetch_add(existing_size, Ordering::SeqCst);
        }

        let response = request
            .send()
            .map_err(|e| DownloadError::NetworkError(e.to_string()))?;

        let status = response.status().as_u16();
        if status >= 400 {
            return Err(DownloadError::HttpError {
                status,
                message: response.status().to_string(),
            });
        }

        // Check if server honored Range request
        // 206 = Partial Content (resume works), 200 = Full file (server ignored Range)
        let actually_resuming = existing_size > 0 && status == 206;
        if existing_size > 0 && status == 200 {
            // Server doesn't support Range, reset byte counter and start fresh
            bytes_downloaded.fetch_sub(existing_size, Ordering::SeqCst);
        }

        // Open file in append or create mode
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(actually_resuming)
            .write(true)
            .truncate(!actually_resuming)
            .open(path)?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Stream download with progress updates
        let mut reader = BufReader::new(response);
        let mut buffer = [0u8; 8192];
        let start = Instant::now();

        loop {
            if self.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }

            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }

            file.write_all(&buffer[..n])?;
            bytes_downloaded.fetch_add(n as u64, Ordering::SeqCst);

            // Report progress
            if let Some(callback) = on_progress {
                let total_downloaded = bytes_downloaded.load(Ordering::SeqCst);
                let file_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

                let progress_pct = if grand_total > 0 {
                    ((total_downloaded as f64 / grand_total as f64) * 100.0) as u8
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

            // Check timeout
            if start.elapsed() > self.file_timeout {
                return Err(DownloadError::Timeout);
            }
        }

        file.sync_all()?;
        Ok(())
    }

    /// Atomically install downloaded files.
    ///
    /// Uses a backup-rename-cleanup pattern to minimize the window where no model exists:
    /// 1. Move existing target to backup (if present)
    /// 2. Rename temp to target
    /// 3. Remove backup on success, or restore on failure
    fn atomic_install(&self) -> Result<(), DownloadError> {
        // Fix: Use safer backup path construction that appends .bak instead of replacing extension.
        // This handles cases like "model.v2" correctly (-> "model.v2.bak", not "model.bak").
        let backup_dir = if let Some(name) = self.target_dir.file_name() {
            let mut p = self.target_dir.clone();
            let new_name = format!("{}.bak", name.to_string_lossy());
            p.set_file_name(new_name);
            p
        } else {
            self.target_dir.with_extension("bak")
        };

        // Clean up any stale backup from previous failed install
        if backup_dir.exists() {
            let _ = fs::remove_dir_all(&backup_dir);
        }

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
                // Success: remove backup
                if had_existing {
                    let _ = fs::remove_dir_all(&backup_dir);
                }
            }
            Err(e) => {
                // Failed: try to restore from backup
                if had_existing && backup_dir.exists() {
                    let _ = fs::rename(&backup_dir, &self.target_dir);
                }
                return Err(e.into());
            }
        }

        // Sync directory
        if let Some(parent) = self.target_dir.parent()
            && let Ok(dir) = File::open(parent)
        {
            let _ = dir.sync_all();
        }

        Ok(())
    }

    /// Write .verified marker file.
    fn write_verified_marker(&self, manifest: &ModelManifest) -> Result<(), DownloadError> {
        let marker_path = self.target_dir.join(".verified");
        let content = format!(
            "revision={}\nverified_at={}\n",
            manifest.revision,
            chrono::Utc::now().to_rfc3339()
        );
        fs::write(marker_path, content)?;
        Ok(())
    }

    /// Clean up temporary download directory.
    fn cleanup_temp(&self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
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

/// Check if a model is installed and verified.
pub fn check_model_installed(model_dir: &Path) -> ModelState {
    if !model_dir.is_dir() {
        return ModelState::NotInstalled;
    }

    let verified_marker = model_dir.join(".verified");
    if !verified_marker.is_file() {
        return ModelState::NotInstalled;
    }

    // Check if all required files exist
    let required = [
        "model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ];
    for file in required {
        if !model_dir.join(file).is_file() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_check_model_installed_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("nonexistent");
        assert_eq!(check_model_installed(&model_dir), ModelState::NotInstalled);
    }

    #[test]
    fn test_check_model_installed_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        // Use fixture files instead of fake content - only copy model.onnx
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models");
        fs::create_dir_all(&model_dir).unwrap();
        fs::copy(fixture_dir.join("model.onnx"), model_dir.join("model.onnx")).unwrap();
        assert_eq!(check_model_installed(&model_dir), ModelState::NotInstalled);
    }

    #[test]
    fn test_check_model_installed_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        // Use fixture files instead of fake content
        copy_model_fixtures(&model_dir).unwrap();
        fs::write(model_dir.join(".verified"), "revision=test\n").unwrap();
        assert_eq!(check_model_installed(&model_dir), ModelState::Ready);
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
    fn test_download_error_display() {
        let err = DownloadError::NetworkError("connection refused".into());
        assert!(err.to_string().contains("network error"));

        let err = DownloadError::VerificationFailed {
            file: "test.onnx".into(),
            expected: "abc".into(),
            actual: "def".into(),
        };
        assert!(err.to_string().contains("verification failed"));
        assert!(err.to_string().contains("test.onnx"));

        let err = DownloadError::ManifestNotVerified {
            model_id: "test-model".into(),
            unverified_files: vec!["model.onnx".into(), "config.json".into()],
            revision_unpinned: true,
        };
        let msg = err.to_string();
        assert!(msg.contains("test-model"));
        assert!(msg.contains("not production-ready"));
        assert!(msg.contains("2 file(s)"));
        assert!(msg.contains("revision is not pinned"));
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
    fn test_manifest_not_production_ready_bakeoff() {
        // Bake-off candidates should NOT be production-ready (placeholder checksums)
        let manifest = ModelManifest::embeddinggemma();
        assert!(!manifest.has_verified_checksums());
        assert!(!manifest.has_pinned_revision());
        assert!(!manifest.is_production_ready());

        // Check that at least one file has the placeholder
        let has_placeholder = manifest
            .files
            .iter()
            .any(|f| f.sha256 == super::PLACEHOLDER_CHECKSUM);
        assert!(has_placeholder);
    }

    #[test]
    fn test_bakeoff_candidates_separation() {
        // All bake-off candidates should have placeholder checksums currently
        let pending = ModelManifest::bakeoff_candidates_pending();
        let ready = ModelManifest::bakeoff_candidates_ready();

        // Currently all bake-off models are pending (none have real checksums)
        assert!(!pending.is_empty());
        // When models get verified, they'll move to the ready list
        assert!(ready.is_empty()); // All bake-off candidates have placeholders

        // Total should equal all candidates
        let all = ModelManifest::bakeoff_candidates();
        assert_eq!(pending.len() + ready.len(), all.len());
    }

    #[test]
    fn test_downloader_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));

        assert!(!downloader.is_cancelled());
        downloader.cancel();
        assert!(downloader.is_cancelled());
    }
}
