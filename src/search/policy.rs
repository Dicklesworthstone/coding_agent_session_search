//! Semantic policy contract for cass hybrid search.
//!
//! This module is the **single source of truth** for all semantic search policy
//! decisions.  Downstream beads (asset manifests, backfill scheduler, model
//! acquisition, configuration surfaces, capability reporting) implement against
//! the types and constants defined here rather than guessing or hardcoding their
//! own values.
//!
//! # Product contract
//!
//! Ordinary search **always works lexically**.  Semantic quality improves
//! opportunistically: when model files are present, vectors are built in the
//! background and hybrid results are blended in.  A missing or broken semantic
//! tier never blocks or degrades lexical search.
//!
//! # Precedence (lowest to highest)
//!
//! 1. **Compiled defaults** — [`SemanticPolicy::compiled_defaults`]
//! 2. **Persisted config** — `~/.config/cass/semantic.toml` (planned)
//! 3. **Environment variables** — `CASS_SEMANTIC_*`
//! 4. **CLI flags** — `--semantic-mode`, `--semantic-budget-mb`, etc.
//!
//! Higher layers override lower layers field-by-field; unset fields inherit.
//!
//! # Behaviour modes
//!
//! | Mode | Lexical | Fast-tier semantic | Quality-tier semantic |
//! |------|---------|--------------------|----------------------|
//! | `HybridPreferred` (default) | always | if available | if model present |
//! | `LexicalOnly` | always | never | never |
//! | `StrictSemantic` | always (floor) | required | required |
//!
//! `StrictSemantic` is for callers that want hard guarantees about semantic
//! quality (e.g., bake-off).  It is never the default.
//!
//! # Storage budget
//!
//! Semantic artifacts are **derivative** — they can always be rebuilt from the
//! canonical SQLite database.  They must never crowd out the DB or the required
//! lexical index.
//!
//! Eviction order (first to go → last to go):
//! 1. HNSW accelerator indices (`.chsw`)
//! 2. Quality-tier vector index (`.fsvi`)
//! 3. Fast-tier vector index
//! 4. Downloaded model files
//!
//! The lexical index and SQLite DB are **never** evicted.

use std::fmt;

use serde::{Deserialize, Serialize};

// ─── Behaviour mode ────────────────────────────────────────────────────────

/// How aggressively cass pursues semantic search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticMode {
    /// Default.  Lexical always works; semantic blended in when available.
    HybridPreferred,
    /// Lexical only — never build or consult semantic assets.
    LexicalOnly,
    /// Both tiers required.  Errors if semantic is unavailable.
    StrictSemantic,
}

impl Default for SemanticMode {
    fn default() -> Self {
        Self::HybridPreferred
    }
}

impl fmt::Display for SemanticMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HybridPreferred => write!(f, "hybrid_preferred"),
            Self::LexicalOnly => write!(f, "lexical_only"),
            Self::StrictSemantic => write!(f, "strict_semantic"),
        }
    }
}

impl SemanticMode {
    /// Parse from a user-provided string (env, CLI, config).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "hybrid_preferred" | "hybrid" | "default" | "auto" => Some(Self::HybridPreferred),
            "lexical_only" | "lexical" | "lex" | "off" => Some(Self::LexicalOnly),
            "strict_semantic" | "strict" | "semantic" => Some(Self::StrictSemantic),
            _ => None,
        }
    }

    /// Whether semantic assets should be built at all.
    pub fn should_build_semantic(&self) -> bool {
        !matches!(self, Self::LexicalOnly)
    }

    /// Whether search should fail if semantic is unavailable.
    pub fn requires_semantic(&self) -> bool {
        matches!(self, Self::StrictSemantic)
    }
}

// ─── Model download policy ─────────────────────────────────────────────────

/// Whether model downloads are automatic, opt-in, or budget-gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDownloadPolicy {
    /// Never download automatically; user must explicitly request.
    OptIn,
    /// Download if disk budget allows and user has consented once.
    BudgetGated,
    /// Download automatically when needed (not recommended for constrained machines).
    Automatic,
}

impl Default for ModelDownloadPolicy {
    fn default() -> Self {
        Self::OptIn
    }
}

impl fmt::Display for ModelDownloadPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OptIn => write!(f, "opt_in"),
            Self::BudgetGated => write!(f, "budget_gated"),
            Self::Automatic => write!(f, "automatic"),
        }
    }
}

impl ModelDownloadPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "opt_in" | "optin" | "manual" => Some(Self::OptIn),
            "budget_gated" | "budget" | "gated" => Some(Self::BudgetGated),
            "automatic" | "auto" => Some(Self::Automatic),
            _ => None,
        }
    }
}

// ─── Tier identifiers ──────────────────────────────────────────────────────

/// Default fast-tier embedder name (always available, no model files).
pub const DEFAULT_FAST_TIER_EMBEDDER: &str = "hash";

/// Default quality-tier embedder name (requires ML model files).
pub const DEFAULT_QUALITY_TIER_EMBEDDER: &str = "minilm";

/// Default reranker name (requires cross-encoder model files).
pub const DEFAULT_RERANKER: &str = "ms-marco-minilm";

// ─── Dimension defaults ────────────────────────────────────────────────────

/// Fast-tier embedding dimension (hash embedder).
pub const DEFAULT_FAST_DIMENSION: usize = 256;

/// Quality-tier embedding dimension (MiniLM).
pub const DEFAULT_QUALITY_DIMENSION: usize = 384;

/// Quality-tier score weight when blending (0.0-1.0).
pub const DEFAULT_QUALITY_WEIGHT: f32 = 0.7;

/// Maximum documents to refine via quality tier per query.
pub const DEFAULT_MAX_REFINEMENT_DOCS: usize = 100;

// ─── Storage budget defaults ───────────────────────────────────────────────

/// Default total semantic disk budget in megabytes.
///
/// This covers model files + vector indices + HNSW accelerators.
/// 500 MB is generous for a personal archive (MiniLM ≈ 90 MB, vectors
/// scale ~1.5 KB per 1000 messages at f16).  For 100 K messages the
/// vector index is ~150 KB — the models dominate.
pub const DEFAULT_SEMANTIC_BUDGET_MB: u64 = 500;

/// Minimum free disk space (MB) that must remain after semantic writes.
///
/// If semantic writes would leave less than this on the volume, they are
/// skipped.  This protects the canonical DB, lexical index, and OS.
pub const MIN_FREE_DISK_MB: u64 = 200;

/// Model files are the biggest single cost.  Cap per-model.
pub const MAX_MODEL_SIZE_MB: u64 = 300;

// ─── Background scheduler budgets ──────────────────────────────────────────

/// Maximum CPU cores the background backfill worker may saturate.
/// On a typical 4-core dev laptop this is ~25 %.
pub const DEFAULT_MAX_BACKFILL_THREADS: usize = 1;

/// Maximum RSS the backfill worker should target (MB).
/// This is advisory — the embedder ONNX runtime is the main consumer.
pub const DEFAULT_MAX_BACKFILL_RSS_MB: u64 = 256;

/// How long (seconds) the scheduler waits after last user activity before
/// starting background work.  This prevents contention during interactive
/// search or indexing.
pub const DEFAULT_IDLE_DELAY_SECONDS: u64 = 30;

/// Maximum wall-clock seconds for a single background work chunk.
/// The scheduler yields after this to re-check budgets and user activity.
pub const DEFAULT_CHUNK_TIMEOUT_SECONDS: u64 = 120;

// ─── Invalidation / upgrade constants ──────────────────────────────────────

/// Semantic schema version.  Bump when the vector document ID encoding,
/// quantization format, or normalization changes.  A version mismatch
/// forces a full vector rebuild.
pub const SEMANTIC_SCHEMA_VERSION: u32 = 1;

/// Changing the chunking strategy (e.g., max tokens per chunk, overlap)
/// invalidates all existing vectors even if the model is unchanged.
pub const CHUNKING_STRATEGY_VERSION: u32 = 1;

// ─── The policy struct ─────────────────────────────────────────────────────

/// Resolved semantic policy after layering defaults → config → env → CLI.
///
/// Every field has a value — the resolution process fills in defaults for
/// anything not specified by higher layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticPolicy {
    // ── Behaviour ──────────────────────────────────────────────────────
    /// Active semantic mode.
    pub mode: SemanticMode,

    /// Whether model downloads may happen automatically.
    pub download_policy: ModelDownloadPolicy,

    // ── Model selection ────────────────────────────────────────────────
    /// Fast-tier embedder name (e.g., "hash").
    pub fast_tier_embedder: String,

    /// Quality-tier embedder name (e.g., "minilm").
    pub quality_tier_embedder: String,

    /// Reranker name (e.g., "ms-marco-minilm").
    pub reranker: String,

    // ── Dimensions / weights ───────────────────────────────────────────
    /// Fast-tier embedding dimension.
    pub fast_dimension: usize,

    /// Quality-tier embedding dimension.
    pub quality_dimension: usize,

    /// Quality weight for score blending (0.0–1.0).
    pub quality_weight: f32,

    /// Maximum documents refined per query.
    pub max_refinement_docs: usize,

    // ── Storage budget ─────────────────────────────────────────────────
    /// Total disk budget for all semantic artifacts (MB).
    pub semantic_budget_mb: u64,

    /// Minimum free disk that must remain after writes (MB).
    pub min_free_disk_mb: u64,

    /// Maximum single model size (MB).
    pub max_model_size_mb: u64,

    // ── Background scheduler ───────────────────────────────────────────
    /// Max threads for background backfill.
    pub max_backfill_threads: usize,

    /// Max RSS target for backfill worker (MB).
    pub max_backfill_rss_mb: u64,

    /// Idle delay before background work starts (seconds).
    pub idle_delay_seconds: u64,

    /// Max seconds per background work chunk.
    pub chunk_timeout_seconds: u64,

    // ── Versioning ─────────────────────────────────────────────────────
    /// Semantic schema version — mismatch forces rebuild.
    pub semantic_schema_version: u32,

    /// Chunking strategy version — mismatch forces rebuild.
    pub chunking_strategy_version: u32,
}

impl Default for SemanticPolicy {
    fn default() -> Self {
        Self::compiled_defaults()
    }
}

impl SemanticPolicy {
    /// Compiled defaults — lowest precedence.
    pub fn compiled_defaults() -> Self {
        Self {
            mode: SemanticMode::default(),
            download_policy: ModelDownloadPolicy::default(),
            fast_tier_embedder: DEFAULT_FAST_TIER_EMBEDDER.to_owned(),
            quality_tier_embedder: DEFAULT_QUALITY_TIER_EMBEDDER.to_owned(),
            reranker: DEFAULT_RERANKER.to_owned(),
            fast_dimension: DEFAULT_FAST_DIMENSION,
            quality_dimension: DEFAULT_QUALITY_DIMENSION,
            quality_weight: DEFAULT_QUALITY_WEIGHT,
            max_refinement_docs: DEFAULT_MAX_REFINEMENT_DOCS,
            semantic_budget_mb: DEFAULT_SEMANTIC_BUDGET_MB,
            min_free_disk_mb: MIN_FREE_DISK_MB,
            max_model_size_mb: MAX_MODEL_SIZE_MB,
            max_backfill_threads: DEFAULT_MAX_BACKFILL_THREADS,
            max_backfill_rss_mb: DEFAULT_MAX_BACKFILL_RSS_MB,
            idle_delay_seconds: DEFAULT_IDLE_DELAY_SECONDS,
            chunk_timeout_seconds: DEFAULT_CHUNK_TIMEOUT_SECONDS,
            semantic_schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_strategy_version: CHUNKING_STRATEGY_VERSION,
        }
    }

    fn with_env_lookup(mut self, mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        if let Some(val) = lookup("CASS_SEMANTIC_MODE") {
            if let Some(mode) = SemanticMode::parse(&val) {
                self.mode = mode;
            }
        }

        // Legacy alias: CASS_SEMANTIC_EMBEDDER=hash → LexicalOnly is wrong,
        // it means "use hash as fast tier and skip quality".  We translate it
        // into mode=HybridPreferred with hash as fast-tier (which is already
        // the default).  The only actionable value is "hash" which forces
        // HashFallback behaviour.
        if let Some(val) = lookup("CASS_SEMANTIC_EMBEDDER") {
            match val.trim().to_ascii_lowercase().as_str() {
                "hash" => {
                    // User explicitly wants hash-only — disable quality tier
                    // but keep the mode hybrid-preferred so lexical still works.
                    self.quality_tier_embedder = "hash".to_owned();
                }
                other => {
                    // Treat as quality-tier embedder name override.
                    self.quality_tier_embedder = other.to_owned();
                }
            }
        }

        if let Some(val) = lookup("CASS_SEMANTIC_DOWNLOAD_POLICY") {
            if let Some(policy) = ModelDownloadPolicy::parse(&val) {
                self.download_policy = policy;
            }
        }

        if let Some(val) = lookup("CASS_SEMANTIC_BUDGET_MB")
            && let Ok(mb) = val.trim().parse::<u64>()
        {
            self.semantic_budget_mb = mb;
        }

        if let Some(val) = lookup("CASS_SEMANTIC_MIN_FREE_DISK_MB")
            && let Ok(mb) = val.trim().parse::<u64>()
        {
            self.min_free_disk_mb = mb;
        }

        if let Some(val) = lookup("CASS_SEMANTIC_MAX_MODEL_SIZE_MB")
            && let Ok(mb) = val.trim().parse::<u64>()
        {
            self.max_model_size_mb = mb;
        }

        // Two-tier overrides (these already exist; we subsume them here for
        // single-point resolution).
        if let Some(val) = lookup("CASS_TWO_TIER_FAST_DIM")
            && let Ok(dim) = val.trim().parse()
        {
            self.fast_dimension = dim;
        }

        if let Some(val) = lookup("CASS_TWO_TIER_QUALITY_DIM")
            && let Ok(dim) = val.trim().parse()
        {
            self.quality_dimension = dim;
        }

        if let Some(val) = lookup("CASS_TWO_TIER_QUALITY_WEIGHT")
            && let Ok(w) = val.trim().parse::<f32>()
        {
            self.quality_weight = w.clamp(0.0, 1.0);
        }

        if let Some(val) = lookup("CASS_TWO_TIER_MAX_REFINEMENT")
            && let Ok(max) = val.trim().parse()
        {
            self.max_refinement_docs = max;
        }

        if let Some(val) = lookup("CASS_SEMANTIC_MAX_BACKFILL_THREADS")
            && let Ok(n) = val.trim().parse()
        {
            self.max_backfill_threads = n;
        }

        if let Some(val) = lookup("CASS_SEMANTIC_MAX_BACKFILL_RSS_MB")
            && let Ok(mb) = val.trim().parse()
        {
            self.max_backfill_rss_mb = mb;
        }

        if let Some(val) = lookup("CASS_SEMANTIC_IDLE_DELAY_SECONDS")
            && let Ok(s) = val.trim().parse()
        {
            self.idle_delay_seconds = s;
        }

        if let Some(val) = lookup("CASS_SEMANTIC_CHUNK_TIMEOUT_SECONDS")
            && let Ok(s) = val.trim().parse()
        {
            self.chunk_timeout_seconds = s;
        }

        self
    }

    /// Layer environment variables over the current policy.
    ///
    /// Only overrides fields for which env vars are set and parseable.
    pub fn with_env_overrides(self) -> Self {
        self.with_env_lookup(|key| dotenvy::var(key).ok())
    }

    /// Layer explicit CLI overrides.
    ///
    /// Each `Option` is `Some` only when the user passed that flag.
    pub fn with_cli_overrides(mut self, overrides: &CliSemanticOverrides) -> Self {
        if let Some(mode) = overrides.mode {
            self.mode = mode;
        }
        if let Some(budget) = overrides.semantic_budget_mb {
            self.semantic_budget_mb = budget;
        }
        if let Some(ref embedder) = overrides.quality_tier_embedder {
            self.quality_tier_embedder = embedder.clone();
        }
        if let Some(threads) = overrides.max_backfill_threads {
            self.max_backfill_threads = threads;
        }
        self
    }

    /// Full resolution: compiled defaults → env → CLI.
    pub fn resolve(cli: &CliSemanticOverrides) -> Self {
        Self::compiled_defaults()
            .with_env_overrides()
            .with_cli_overrides(cli)
    }
}

/// CLI-level overrides — `None` means "inherit from lower layer".
#[derive(Debug, Clone, Default)]
pub struct CliSemanticOverrides {
    pub mode: Option<SemanticMode>,
    pub semantic_budget_mb: Option<u64>,
    pub quality_tier_embedder: Option<String>,
    pub max_backfill_threads: Option<usize>,
}

// ─── Effective-setting introspection ───────────────────────────────────────

/// Where a configuration value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// Compiled into the binary.
    CompiledDefault,
    /// Loaded from persisted config file.
    Config,
    /// Set via environment variable.
    Environment,
    /// Set via CLI flag.
    Cli,
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompiledDefault => write!(f, "compiled_default"),
            Self::Config => write!(f, "config"),
            Self::Environment => write!(f, "environment"),
            Self::Cli => write!(f, "cli"),
        }
    }
}

/// A single setting with its resolved value and provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveSetting {
    pub name: String,
    pub value: String,
    pub source: SettingSource,
    /// The environment variable that could override this (if any).
    pub env_var: Option<String>,
}

/// Complete effective-settings report for `cass status --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveSettings {
    pub settings: Vec<EffectiveSetting>,
}

impl EffectiveSettings {
    fn resolve_with_env_lookup(
        cli: &CliSemanticOverrides,
        lookup: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let defaults = SemanticPolicy::compiled_defaults();
        let env_policy = defaults.clone().with_env_lookup(lookup);
        let final_policy = env_policy.clone().with_cli_overrides(cli);

        let mut settings = Vec::new();

        // Helper: determine source for a field by comparing layers.
        macro_rules! track {
            ($name:expr, $field:ident, $env_var:expr, $cli_field:ident) => {
                let source = if cli.$cli_field.is_some() {
                    SettingSource::Cli
                } else if env_policy.$field != defaults.$field {
                    SettingSource::Environment
                } else {
                    SettingSource::CompiledDefault
                };
                settings.push(EffectiveSetting {
                    name: $name.to_owned(),
                    value: format!("{}", final_policy.$field),
                    source,
                    env_var: Some($env_var.to_owned()),
                });
            };
        }

        // Mode
        track!("mode", mode, "CASS_SEMANTIC_MODE", mode);

        // Budget
        track!(
            "semantic_budget_mb",
            semantic_budget_mb,
            "CASS_SEMANTIC_BUDGET_MB",
            semantic_budget_mb
        );

        // Quality tier embedder
        track!(
            "quality_tier_embedder",
            quality_tier_embedder,
            "CASS_SEMANTIC_EMBEDDER",
            quality_tier_embedder
        );

        // Backfill threads
        track!(
            "max_backfill_threads",
            max_backfill_threads,
            "CASS_SEMANTIC_MAX_BACKFILL_THREADS",
            max_backfill_threads
        );

        // Fields without CLI overrides — only env vs default.
        // Note: fast_tier_embedder and reranker have no env var overrides.
        settings.push(EffectiveSetting {
            name: "fast_tier_embedder".to_owned(),
            value: final_policy.fast_tier_embedder.clone(),
            source: SettingSource::CompiledDefault,
            env_var: None,
        });
        settings.push(EffectiveSetting {
            name: "reranker".to_owned(),
            value: final_policy.reranker.clone(),
            source: SettingSource::CompiledDefault,
            env_var: None,
        });

        let env_only_fields: &[(&str, &str, fn(&SemanticPolicy) -> String)] = &[
            ("fast_dimension", "CASS_TWO_TIER_FAST_DIM", |p| {
                p.fast_dimension.to_string()
            }),
            ("quality_dimension", "CASS_TWO_TIER_QUALITY_DIM", |p| {
                p.quality_dimension.to_string()
            }),
            ("quality_weight", "CASS_TWO_TIER_QUALITY_WEIGHT", |p| {
                format!("{}", p.quality_weight)
            }),
            ("max_refinement_docs", "CASS_TWO_TIER_MAX_REFINEMENT", |p| {
                p.max_refinement_docs.to_string()
            }),
            ("min_free_disk_mb", "CASS_SEMANTIC_MIN_FREE_DISK_MB", |p| {
                p.min_free_disk_mb.to_string()
            }),
            (
                "max_model_size_mb",
                "CASS_SEMANTIC_MAX_MODEL_SIZE_MB",
                |p| p.max_model_size_mb.to_string(),
            ),
            ("download_policy", "CASS_SEMANTIC_DOWNLOAD_POLICY", |p| {
                p.download_policy.to_string()
            }),
            (
                "idle_delay_seconds",
                "CASS_SEMANTIC_IDLE_DELAY_SECONDS",
                |p| p.idle_delay_seconds.to_string(),
            ),
            (
                "chunk_timeout_seconds",
                "CASS_SEMANTIC_CHUNK_TIMEOUT_SECONDS",
                |p| p.chunk_timeout_seconds.to_string(),
            ),
            (
                "max_backfill_rss_mb",
                "CASS_SEMANTIC_MAX_BACKFILL_RSS_MB",
                |p| p.max_backfill_rss_mb.to_string(),
            ),
        ];

        for (name, env_var, getter) in env_only_fields {
            let default_val = getter(&defaults);
            let env_val = getter(&env_policy);
            let source = if env_val != default_val {
                SettingSource::Environment
            } else {
                SettingSource::CompiledDefault
            };
            settings.push(EffectiveSetting {
                name: name.to_string(),
                value: getter(&final_policy),
                source,
                env_var: Some(env_var.to_string()),
            });
        }

        // Version fields (always compiled default).
        settings.push(EffectiveSetting {
            name: "semantic_schema_version".to_owned(),
            value: final_policy.semantic_schema_version.to_string(),
            source: SettingSource::CompiledDefault,
            env_var: None,
        });
        settings.push(EffectiveSetting {
            name: "chunking_strategy_version".to_owned(),
            value: final_policy.chunking_strategy_version.to_string(),
            source: SettingSource::CompiledDefault,
            env_var: None,
        });

        Self { settings }
    }

    /// Build the effective-settings report by resolving each field with
    /// full provenance tracking.
    pub fn resolve(cli: &CliSemanticOverrides) -> Self {
        Self::resolve_with_env_lookup(cli, |key| dotenvy::var(key).ok())
    }

    /// Find a setting by name.
    pub fn get(&self, name: &str) -> Option<&EffectiveSetting> {
        self.settings.iter().find(|s| s.name == name)
    }

    /// Count settings from each source.
    pub fn source_counts(&self) -> std::collections::HashMap<SettingSource, usize> {
        let mut counts = std::collections::HashMap::new();
        for s in &self.settings {
            *counts.entry(s.source).or_insert(0) += 1;
        }
        counts
    }
}

// ─── Capability classification ─────────────────────────────────────────────

/// What semantic quality level is achievable on this machine right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCapability {
    /// Full quality: ML model present, vector index built, HNSW available.
    FullQuality,
    /// Quality tier available but HNSW accelerator missing (brute-force OK).
    QualityNoHnsw,
    /// Only fast-tier (hash) semantic — no ML model installed.
    FastTierOnly,
    /// No semantic capability — mode is lexical-only.
    LexicalOnly,
    /// Semantic is desired but broken (model corrupt, load failed, etc.).
    Degraded { reason: String },
}

impl SemanticCapability {
    /// Whether any semantic search is possible.
    pub fn can_search_semantic(&self) -> bool {
        matches!(
            self,
            Self::FullQuality | Self::QualityNoHnsw | Self::FastTierOnly
        )
    }

    /// Whether quality-tier (ML) search is possible.
    pub fn has_quality_tier(&self) -> bool {
        matches!(self, Self::FullQuality | Self::QualityNoHnsw)
    }

    /// Short label for TUI/robot status.
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::FullQuality => "SEM+",
            Self::QualityNoHnsw => "SEM",
            Self::FastTierOnly => "SEM*",
            Self::LexicalOnly => "LEX",
            Self::Degraded { .. } => "ERR",
        }
    }

    /// Human-readable summary for `cass status --json`.
    pub fn summary(&self) -> String {
        match self {
            Self::FullQuality => {
                "Full semantic: ML embedder + vector index + HNSW accelerator".to_owned()
            }
            Self::QualityNoHnsw => {
                "Quality semantic: ML embedder + vector index (brute-force)".to_owned()
            }
            Self::FastTierOnly => {
                "Fast semantic: hash embedder only (install ML model for quality)".to_owned()
            }
            Self::LexicalOnly => "Lexical only: semantic search disabled by policy".to_owned(),
            Self::Degraded { reason } => format!("Degraded: {reason}"),
        }
    }
}

// ─── Invalidation decisions ────────────────────────────────────────────────

/// What happened and what to do about existing semantic assets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidationAction {
    /// Assets are current — nothing to do.
    UpToDate,
    /// Vectors are stale but usable until rebuild completes.
    RebuildInBackground,
    /// Vectors are from an incompatible schema — must discard and rebuild.
    DiscardAndRebuild { reason: String },
    /// Assets should be removed entirely (mode changed to lexical-only).
    Evict,
}

/// Metadata stored alongside semantic assets to detect invalidation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticAssetManifest {
    /// Embedder ID that produced these vectors (e.g., "minilm-384").
    pub embedder_id: String,
    /// HuggingFace revision hash of the model checkpoint.
    pub model_revision: String,
    /// Semantic schema version at build time.
    pub schema_version: u32,
    /// Chunking strategy version at build time.
    pub chunking_version: u32,
    /// Number of documents embedded.
    pub doc_count: u64,
    /// Unix timestamp (ms) of last build.
    pub built_at_ms: i64,
}

impl SemanticAssetManifest {
    /// Decide what to do given the current policy, expected embedder ID, and
    /// the model revision currently installed.
    ///
    /// `expected_embedder_id` should be the full embedder ID for the tier this
    /// manifest belongs to (e.g., `"fnv1a-384"` for fast, `"minilm-384"` for
    /// quality).
    pub fn invalidation_action(
        &self,
        policy: &SemanticPolicy,
        current_model_revision: &str,
        expected_embedder_id: &str,
    ) -> InvalidationAction {
        // Mode changed to lexical-only → evict everything.
        if !policy.mode.should_build_semantic() {
            return InvalidationAction::Evict;
        }

        // Schema version mismatch → hard rebuild (encoding changed).
        if self.schema_version != policy.semantic_schema_version {
            return InvalidationAction::DiscardAndRebuild {
                reason: format!(
                    "semantic schema version changed ({} → {})",
                    self.schema_version, policy.semantic_schema_version
                ),
            };
        }

        // Chunking strategy changed → hard rebuild (segments differ).
        if self.chunking_version != policy.chunking_strategy_version {
            return InvalidationAction::DiscardAndRebuild {
                reason: format!(
                    "chunking strategy version changed ({} → {})",
                    self.chunking_version, policy.chunking_strategy_version
                ),
            };
        }

        // Embedder ID changed entirely (e.g., minilm → snowflake) → hard
        // rebuild because dimensions or encoding may differ.  This MUST be
        // checked before model revision: an embedder change means the vectors
        // are in a completely different space and cannot serve as interim results.
        if self.embedder_id != expected_embedder_id {
            return InvalidationAction::DiscardAndRebuild {
                reason: format!(
                    "embedder changed ({} → {})",
                    self.embedder_id, expected_embedder_id
                ),
            };
        }

        // Model revision changed (same embedder) → soft rebuild.  Old vectors
        // are in the same space and usable until rebuild completes.
        if self.model_revision != current_model_revision {
            return InvalidationAction::RebuildInBackground;
        }

        InvalidationAction::UpToDate
    }
}

// ─── Budget decisions ──────────────────────────────────────────────────────

/// Result of a disk-budget check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDecision {
    /// Plenty of room — proceed.
    Allowed,
    /// Would exceed the semantic budget but free disk is fine — warn.
    OverBudgetWarn { used_mb: u64, budget_mb: u64 },
    /// Would leave less than min_free_disk_mb — deny.
    DiskPressureDeny { free_mb: u64, min_required_mb: u64 },
    /// Model too large for per-model cap — deny.
    ModelTooLarge { model_mb: u64, max_mb: u64 },
}

impl BudgetDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed | Self::OverBudgetWarn { .. })
    }
}

impl SemanticPolicy {
    /// Check whether a proposed write of `write_size_mb` is within budget.
    ///
    /// This is intended for **model downloads** — the first check compares
    /// against `max_model_size_mb`.  For vector index writes (which are much
    /// smaller), prefer skipping the per-model cap or calling with a separate
    /// budget method when one is needed.
    ///
    /// `current_semantic_usage_mb` is the total disk used by semantic artifacts
    /// right now.  `free_disk_mb` is the free space on the volume.
    pub fn check_budget(
        &self,
        write_size_mb: u64,
        current_semantic_usage_mb: u64,
        free_disk_mb: u64,
    ) -> BudgetDecision {
        // Per-model cap.
        if write_size_mb > self.max_model_size_mb {
            return BudgetDecision::ModelTooLarge {
                model_mb: write_size_mb,
                max_mb: self.max_model_size_mb,
            };
        }

        // Free disk floor.
        if free_disk_mb.saturating_sub(write_size_mb) < self.min_free_disk_mb {
            return BudgetDecision::DiskPressureDeny {
                free_mb: free_disk_mb,
                min_required_mb: self.min_free_disk_mb,
            };
        }

        // Total semantic budget.
        let new_total = current_semantic_usage_mb.saturating_add(write_size_mb);
        if new_total > self.semantic_budget_mb {
            return BudgetDecision::OverBudgetWarn {
                used_mb: new_total,
                budget_mb: self.semantic_budget_mb,
            };
        }

        BudgetDecision::Allowed
    }
}

// ─── Robot-friendly capability payload ─────────────────────────────────────

/// JSON-serializable capability snapshot for `cass status --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticCapabilityReport {
    pub mode: SemanticMode,
    pub capability: SemanticCapability,
    pub fast_tier_embedder: String,
    pub quality_tier_embedder: String,
    pub reranker: String,
    pub fast_dimension: usize,
    pub quality_dimension: usize,
    pub quality_weight: f32,
    pub semantic_budget_mb: u64,
    pub current_usage_mb: u64,
    pub download_policy: ModelDownloadPolicy,
    pub semantic_schema_version: u32,
    pub chunking_strategy_version: u32,
    pub summary: String,
}

impl SemanticCapabilityReport {
    /// Build a report from a resolved policy and observed capability.
    pub fn from_policy(
        policy: &SemanticPolicy,
        capability: SemanticCapability,
        current_usage_mb: u64,
    ) -> Self {
        let summary = capability.summary();
        Self {
            mode: policy.mode,
            capability,
            fast_tier_embedder: policy.fast_tier_embedder.clone(),
            quality_tier_embedder: policy.quality_tier_embedder.clone(),
            reranker: policy.reranker.clone(),
            fast_dimension: policy.fast_dimension,
            quality_dimension: policy.quality_dimension,
            quality_weight: policy.quality_weight,
            semantic_budget_mb: policy.semantic_budget_mb,
            current_usage_mb,
            download_policy: policy.download_policy,
            semantic_schema_version: policy.semantic_schema_version,
            chunking_strategy_version: policy.chunking_strategy_version,
            summary,
        }
    }
}

// ─── Eviction order ────────────────────────────────────────────────────────

/// Ordered list of semantic artifact categories, first-to-evict first.
pub const EVICTION_ORDER: &[SemanticArtifactKind] = &[
    SemanticArtifactKind::HnswAccelerator,
    SemanticArtifactKind::QualityVectorIndex,
    SemanticArtifactKind::FastVectorIndex,
    SemanticArtifactKind::ModelFiles,
];

/// Categories of semantic artifacts for eviction / budget accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticArtifactKind {
    HnswAccelerator,
    QualityVectorIndex,
    FastVectorIndex,
    ModelFiles,
}

impl SemanticArtifactKind {
    /// Whether this artifact is required for the given capability level.
    pub fn required_for(&self, capability: &SemanticCapability) -> bool {
        match (self, capability) {
            (_, SemanticCapability::LexicalOnly) => false,
            (Self::HnswAccelerator, _) => false, // always optional
            (Self::ModelFiles, SemanticCapability::FastTierOnly) => false,
            (Self::QualityVectorIndex, SemanticCapability::FastTierOnly) => false,
            _ => true,
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Precedence resolution ──────────────────────────────────────────

    #[test]
    fn compiled_defaults_are_hybrid_preferred() {
        let p = SemanticPolicy::compiled_defaults();
        assert_eq!(p.mode, SemanticMode::HybridPreferred);
        assert_eq!(p.fast_tier_embedder, "hash");
        assert_eq!(p.quality_tier_embedder, "minilm");
        assert_eq!(p.download_policy, ModelDownloadPolicy::OptIn);
        assert_eq!(p.fast_dimension, 256);
        assert_eq!(p.quality_dimension, 384);
        assert!((p.quality_weight - 0.7).abs() < f32::EPSILON);
        assert_eq!(p.max_refinement_docs, 100);
        assert_eq!(p.semantic_budget_mb, 500);
        assert_eq!(p.min_free_disk_mb, 200);
        assert_eq!(p.max_backfill_threads, 1);
        assert_eq!(p.semantic_schema_version, SEMANTIC_SCHEMA_VERSION);
        assert_eq!(p.chunking_strategy_version, CHUNKING_STRATEGY_VERSION);
    }

    #[test]
    fn cli_overrides_beat_defaults() {
        let cli = CliSemanticOverrides {
            mode: Some(SemanticMode::LexicalOnly),
            semantic_budget_mb: Some(100),
            quality_tier_embedder: Some("snowflake".to_owned()),
            max_backfill_threads: Some(4),
        };
        let p = SemanticPolicy::compiled_defaults().with_cli_overrides(&cli);
        assert_eq!(p.mode, SemanticMode::LexicalOnly);
        assert_eq!(p.semantic_budget_mb, 100);
        assert_eq!(p.quality_tier_embedder, "snowflake");
        assert_eq!(p.max_backfill_threads, 4);
        // Unset fields remain default.
        assert_eq!(p.fast_tier_embedder, "hash");
        assert_eq!(p.quality_dimension, 384);
    }

    #[test]
    fn cli_overrides_beat_env_overrides() {
        // Simulate env setting mode=lexical_only, then CLI overrides to strict.
        let mut p = SemanticPolicy::compiled_defaults();
        p.mode = SemanticMode::LexicalOnly; // as-if env set it
        let cli = CliSemanticOverrides {
            mode: Some(SemanticMode::StrictSemantic),
            ..Default::default()
        };
        let p = p.with_cli_overrides(&cli);
        assert_eq!(p.mode, SemanticMode::StrictSemantic);
    }

    // ── Semantic mode parsing (table-driven) ───────────────────────────

    #[test]
    fn semantic_mode_parsing() {
        let cases: &[(&str, Option<SemanticMode>)] = &[
            ("hybrid_preferred", Some(SemanticMode::HybridPreferred)),
            ("hybrid", Some(SemanticMode::HybridPreferred)),
            ("default", Some(SemanticMode::HybridPreferred)),
            ("auto", Some(SemanticMode::HybridPreferred)),
            ("HYBRID", Some(SemanticMode::HybridPreferred)),
            ("lexical_only", Some(SemanticMode::LexicalOnly)),
            ("lexical", Some(SemanticMode::LexicalOnly)),
            ("lex", Some(SemanticMode::LexicalOnly)),
            ("off", Some(SemanticMode::LexicalOnly)),
            ("strict_semantic", Some(SemanticMode::StrictSemantic)),
            ("strict", Some(SemanticMode::StrictSemantic)),
            ("semantic", Some(SemanticMode::StrictSemantic)),
            ("  Hybrid-Preferred  ", Some(SemanticMode::HybridPreferred)),
            ("nonsense", None),
            ("", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                SemanticMode::parse(input),
                *expected,
                "failed for input: {input:?}"
            );
        }
    }

    #[test]
    fn download_policy_parsing() {
        let cases: &[(&str, Option<ModelDownloadPolicy>)] = &[
            ("opt_in", Some(ModelDownloadPolicy::OptIn)),
            ("optin", Some(ModelDownloadPolicy::OptIn)),
            ("manual", Some(ModelDownloadPolicy::OptIn)),
            ("budget_gated", Some(ModelDownloadPolicy::BudgetGated)),
            ("budget", Some(ModelDownloadPolicy::BudgetGated)),
            ("gated", Some(ModelDownloadPolicy::BudgetGated)),
            ("automatic", Some(ModelDownloadPolicy::Automatic)),
            ("auto", Some(ModelDownloadPolicy::Automatic)),
            ("xyz", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                ModelDownloadPolicy::parse(input),
                *expected,
                "failed for input: {input:?}"
            );
        }
    }

    // ── Semantic mode behaviour flags ──────────────────────────────────

    #[test]
    fn mode_behaviour_flags() {
        let cases: &[(SemanticMode, bool, bool)] = &[
            // (mode, should_build_semantic, requires_semantic)
            (SemanticMode::HybridPreferred, true, false),
            (SemanticMode::LexicalOnly, false, false),
            (SemanticMode::StrictSemantic, true, true),
        ];
        for (mode, build, require) in cases {
            assert_eq!(
                mode.should_build_semantic(),
                *build,
                "should_build for {mode:?}"
            );
            assert_eq!(mode.requires_semantic(), *require, "requires for {mode:?}");
        }
    }

    // ── Capability classification ──────────────────────────────────────

    #[test]
    fn capability_classification() {
        let cases: &[(SemanticCapability, bool, bool, &str)] = &[
            // (capability, can_search, has_quality, label)
            (SemanticCapability::FullQuality, true, true, "SEM+"),
            (SemanticCapability::QualityNoHnsw, true, true, "SEM"),
            (SemanticCapability::FastTierOnly, true, false, "SEM*"),
            (SemanticCapability::LexicalOnly, false, false, "LEX"),
            (
                SemanticCapability::Degraded {
                    reason: "test".to_owned(),
                },
                false,
                false,
                "ERR",
            ),
        ];
        for (cap, can_search, has_quality, label) in cases {
            assert_eq!(
                cap.can_search_semantic(),
                *can_search,
                "can_search for {cap:?}"
            );
            assert_eq!(
                cap.has_quality_tier(),
                *has_quality,
                "has_quality for {cap:?}"
            );
            assert_eq!(cap.status_label(), *label, "label for {cap:?}");
        }
    }

    // ── Budget decisions (table-driven) ────────────────────────────────

    #[test]
    fn budget_decisions() {
        let p = SemanticPolicy::compiled_defaults();
        // defaults: budget=500, min_free=200, max_model=300

        let cases: &[(u64, u64, u64, BudgetDecision)] = &[
            // (write_mb, current_usage_mb, free_disk_mb, expected)
            //
            // Normal: 90 MB write, 100 used, 1000 free → allowed
            (90, 100, 1000, BudgetDecision::Allowed),
            // Over budget: 90 MB write, 450 used (total=540 > 500) → warn
            (
                90,
                450,
                1000,
                BudgetDecision::OverBudgetWarn {
                    used_mb: 540,
                    budget_mb: 500,
                },
            ),
            // Disk pressure: 90 MB write, 0 used, 250 free (250-90=160 < 200) → deny
            (
                90,
                0,
                250,
                BudgetDecision::DiskPressureDeny {
                    free_mb: 250,
                    min_required_mb: 200,
                },
            ),
            // Model too large: 350 MB > max_model 300 → deny
            (
                350,
                0,
                1000,
                BudgetDecision::ModelTooLarge {
                    model_mb: 350,
                    max_mb: 300,
                },
            ),
            // Edge: exact budget limit (90+410=500) → allowed
            (90, 410, 1000, BudgetDecision::Allowed),
            // Edge: 1 MB over budget → warn
            (
                91,
                410,
                1000,
                BudgetDecision::OverBudgetWarn {
                    used_mb: 501,
                    budget_mb: 500,
                },
            ),
            // Edge: exact free floor (free - write = min_free exactly)
            (90, 0, 290, BudgetDecision::Allowed),
            // Edge: 1 MB under free floor
            (
                90,
                0,
                289,
                BudgetDecision::DiskPressureDeny {
                    free_mb: 289,
                    min_required_mb: 200,
                },
            ),
        ];

        for (write, usage, free, expected) in cases {
            let got = p.check_budget(*write, *usage, *free);
            assert_eq!(
                got, *expected,
                "budget check failed for write={write}, usage={usage}, free={free}"
            );
        }
    }

    // ── Invalidation / upgrade decisions (table-driven) ────────────────

    #[test]
    fn invalidation_decisions() {
        let policy = SemanticPolicy::compiled_defaults();
        let expected_id = format!(
            "{}-{}",
            policy.quality_tier_embedder, policy.quality_dimension
        );

        let base_manifest = SemanticAssetManifest {
            embedder_id: expected_id.clone(),
            model_revision: "abc123".to_owned(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            doc_count: 1000,
            built_at_ms: 1700000000000,
        };

        // Case 1: Everything matches → UpToDate
        assert_eq!(
            base_manifest.invalidation_action(&policy, "abc123", &expected_id),
            InvalidationAction::UpToDate,
        );

        // Case 2: Model revision changed → soft rebuild
        assert_eq!(
            base_manifest.invalidation_action(&policy, "def456", &expected_id),
            InvalidationAction::RebuildInBackground,
        );

        // Case 3: Schema version changed → hard rebuild
        {
            let mut m = base_manifest.clone();
            m.schema_version = 0;
            let action = m.invalidation_action(&policy, "abc123", &expected_id);
            assert!(matches!(
                action,
                InvalidationAction::DiscardAndRebuild { .. }
            ));
        }

        // Case 4: Chunking version changed → hard rebuild
        {
            let mut m = base_manifest.clone();
            m.chunking_version = 0;
            let action = m.invalidation_action(&policy, "abc123", &expected_id);
            assert!(matches!(
                action,
                InvalidationAction::DiscardAndRebuild { .. }
            ));
        }

        // Case 5: Embedder ID changed → hard rebuild
        {
            let mut m = base_manifest.clone();
            m.embedder_id = "snowflake-768".to_owned();
            let action = m.invalidation_action(&policy, "abc123", &expected_id);
            assert!(matches!(
                action,
                InvalidationAction::DiscardAndRebuild { .. }
            ));
        }

        // Case 6: Mode changed to lexical-only → evict
        {
            let mut lex_policy = policy.clone();
            lex_policy.mode = SemanticMode::LexicalOnly;
            assert_eq!(
                base_manifest.invalidation_action(&lex_policy, "abc123", &expected_id),
                InvalidationAction::Evict,
            );
        }
    }

    // ── Eviction order ─────────────────────────────────────────────────

    #[test]
    fn eviction_order_hnsw_first_model_last() {
        assert_eq!(EVICTION_ORDER[0], SemanticArtifactKind::HnswAccelerator);
        assert_eq!(EVICTION_ORDER[1], SemanticArtifactKind::QualityVectorIndex);
        assert_eq!(EVICTION_ORDER[2], SemanticArtifactKind::FastVectorIndex);
        assert_eq!(EVICTION_ORDER[3], SemanticArtifactKind::ModelFiles);
    }

    #[test]
    fn artifact_required_for_capability() {
        use SemanticArtifactKind::*;
        use SemanticCapability::*;

        let cases: &[(SemanticArtifactKind, SemanticCapability, bool)] = &[
            // HNSW is never required
            (HnswAccelerator, FullQuality, false),
            (HnswAccelerator, FastTierOnly, false),
            (HnswAccelerator, LexicalOnly, false),
            // Nothing required for lexical-only
            (ModelFiles, LexicalOnly, false),
            (QualityVectorIndex, LexicalOnly, false),
            (FastVectorIndex, LexicalOnly, false),
            // FastTierOnly needs fast index but not model/quality
            (FastVectorIndex, FastTierOnly, true),
            (QualityVectorIndex, FastTierOnly, false),
            (ModelFiles, FastTierOnly, false),
            // FullQuality needs everything except HNSW
            (ModelFiles, FullQuality, true),
            (QualityVectorIndex, FullQuality, true),
            (FastVectorIndex, FullQuality, true),
        ];

        for (artifact, cap, expected) in cases {
            assert_eq!(
                artifact.required_for(cap),
                *expected,
                "{artifact:?} required_for {cap:?}"
            );
        }
    }

    // ── Robot-friendly fixture payloads ─────────────────────────────────

    #[test]
    fn fixture_no_model_state() {
        let policy = SemanticPolicy::compiled_defaults();
        let cap = SemanticCapability::FastTierOnly;
        let report = SemanticCapabilityReport::from_policy(&policy, cap, 0);

        assert_eq!(report.mode, SemanticMode::HybridPreferred);
        assert!(report.summary.contains("hash embedder only"));
        assert_eq!(report.current_usage_mb, 0);

        // Verify serialization round-trips.
        let json = serde_json::to_string_pretty(&report).unwrap();
        let deser: SemanticCapabilityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.mode, report.mode);
        assert_eq!(deser.fast_tier_embedder, "hash");
    }

    #[test]
    fn fixture_fast_tier_only_state() {
        let policy = SemanticPolicy::compiled_defaults();
        let cap = SemanticCapability::FastTierOnly;
        let report = SemanticCapabilityReport::from_policy(&policy, cap, 0);

        assert_eq!(report.capability, SemanticCapability::FastTierOnly);
        assert_eq!(report.quality_tier_embedder, "minilm");
        assert_eq!(report.download_policy, ModelDownloadPolicy::OptIn);
    }

    #[test]
    fn fixture_full_quality_state() {
        let policy = SemanticPolicy::compiled_defaults();
        let cap = SemanticCapability::FullQuality;
        let report = SemanticCapabilityReport::from_policy(&policy, cap, 95);

        assert_eq!(report.capability, SemanticCapability::FullQuality);
        assert_eq!(report.current_usage_mb, 95);
        assert!(report.summary.contains("Full semantic"));

        let json = serde_json::to_string_pretty(&report).unwrap();
        let deser: SemanticCapabilityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.current_usage_mb, 95);
    }

    // ── Serialization round-trip ───────────────────────────────────────

    #[test]
    fn policy_json_round_trip() {
        let policy = SemanticPolicy::compiled_defaults();
        let json = serde_json::to_string(&policy).unwrap();
        let deser: SemanticPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(deser, policy);
    }

    #[test]
    fn asset_manifest_json_round_trip() {
        let manifest = SemanticAssetManifest {
            embedder_id: "minilm-384".to_owned(),
            model_revision: "abc123".to_owned(),
            schema_version: 1,
            chunking_version: 1,
            doc_count: 5000,
            built_at_ms: 1700000000000,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let deser: SemanticAssetManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser, manifest);
    }

    // ── Effective-settings introspection ────────────────────────────────

    #[test]
    fn effective_settings_all_defaults() {
        let cli = CliSemanticOverrides::default();
        let settings = EffectiveSettings::resolve(&cli);

        // All settings should exist.
        assert!(settings.settings.len() >= 15);

        // All should be compiled defaults (no env or CLI set).
        for s in &settings.settings {
            assert_eq!(
                s.source,
                SettingSource::CompiledDefault,
                "setting '{}' should be CompiledDefault, got {:?}",
                s.name,
                s.source
            );
        }

        // Verify specific values.
        let mode = settings.get("mode").unwrap();
        assert_eq!(mode.value, "hybrid_preferred");

        let budget = settings.get("semantic_budget_mb").unwrap();
        assert_eq!(budget.value, "500");

        // Verify all policy fields are represented, including those
        // without env vars.
        assert!(settings.get("fast_tier_embedder").is_some());
        assert!(settings.get("reranker").is_some());
        assert_eq!(settings.get("reranker").unwrap().value, "ms-marco-minilm");
    }

    #[test]
    fn effective_settings_cli_overrides_show_cli_source() {
        let cli = CliSemanticOverrides {
            mode: Some(SemanticMode::LexicalOnly),
            semantic_budget_mb: Some(100),
            ..Default::default()
        };
        let settings = EffectiveSettings::resolve(&cli);

        let mode = settings.get("mode").unwrap();
        assert_eq!(mode.value, "lexical_only");
        assert_eq!(mode.source, SettingSource::Cli);

        let budget = settings.get("semantic_budget_mb").unwrap();
        assert_eq!(budget.value, "100");
        assert_eq!(budget.source, SettingSource::Cli);

        // Non-overridden fields remain default.
        let fast_dim = settings.get("fast_dimension").unwrap();
        assert_eq!(fast_dim.source, SettingSource::CompiledDefault);
    }

    #[test]
    fn effective_settings_lookup_by_name() {
        let cli = CliSemanticOverrides::default();
        let settings = EffectiveSettings::resolve(&cli);

        assert!(settings.get("mode").is_some());
        assert!(settings.get("semantic_schema_version").is_some());
        assert!(settings.get("nonexistent").is_none());
    }

    #[test]
    fn effective_settings_environment_overrides_show_environment_source() {
        let settings =
            EffectiveSettings::resolve_with_env_lookup(&CliSemanticOverrides::default(), |key| {
                match key {
                    "CASS_SEMANTIC_MODE" => Some("lexical_only".to_string()),
                    "CASS_SEMANTIC_BUDGET_MB" => Some("321".to_string()),
                    _ => None,
                }
            });

        let mode = settings.get("mode").unwrap();
        assert_eq!(mode.value, "lexical_only");
        assert_eq!(mode.source, SettingSource::Environment);

        let budget = settings.get("semantic_budget_mb").unwrap();
        assert_eq!(budget.value, "321");
        assert_eq!(budget.source, SettingSource::Environment);
    }

    #[test]
    fn effective_settings_download_policy_uses_snake_case_value() {
        let settings =
            EffectiveSettings::resolve_with_env_lookup(&CliSemanticOverrides::default(), |key| {
                match key {
                    "CASS_SEMANTIC_DOWNLOAD_POLICY" => Some("budget_gated".to_string()),
                    _ => None,
                }
            });

        let policy = settings.get("download_policy").unwrap();
        assert_eq!(policy.value, "budget_gated");
        assert_eq!(policy.source, SettingSource::Environment);
    }

    #[test]
    fn effective_settings_json_round_trip() {
        let cli = CliSemanticOverrides {
            mode: Some(SemanticMode::StrictSemantic),
            ..Default::default()
        };
        let settings = EffectiveSettings::resolve(&cli);
        let json = serde_json::to_string_pretty(&settings).unwrap();
        let deser: EffectiveSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.settings.len(), settings.settings.len());
        assert_eq!(deser.get("mode").unwrap().value, "strict_semantic");
    }

    #[test]
    fn effective_settings_source_counts() {
        let cli = CliSemanticOverrides {
            mode: Some(SemanticMode::LexicalOnly),
            semantic_budget_mb: Some(200),
            ..Default::default()
        };
        let settings = EffectiveSettings::resolve(&cli);
        let counts = settings.source_counts();

        assert_eq!(*counts.get(&SettingSource::Cli).unwrap_or(&0), 2);
        // Everything else is compiled default.
        assert!(*counts.get(&SettingSource::CompiledDefault).unwrap_or(&0) > 10);
    }

    #[test]
    fn effective_settings_version_fields_always_compiled() {
        let cli = CliSemanticOverrides::default();
        let settings = EffectiveSettings::resolve(&cli);

        let schema = settings.get("semantic_schema_version").unwrap();
        assert_eq!(schema.source, SettingSource::CompiledDefault);
        assert!(schema.env_var.is_none()); // not overridable

        let chunking = settings.get("chunking_strategy_version").unwrap();
        assert_eq!(chunking.source, SettingSource::CompiledDefault);
        assert!(chunking.env_var.is_none());
    }
}
