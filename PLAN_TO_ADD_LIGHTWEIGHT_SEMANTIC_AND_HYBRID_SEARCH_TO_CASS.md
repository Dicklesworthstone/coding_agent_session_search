# Plan: Lightweight Semantic & Hybrid Search for CASS

## Executive Summary

This plan adds **true semantic search** and **hybrid search with RRF reranking** to `cass`, allowing users to cycle through three search modes via a keyboard shortcut:

1. **Lexical** (current default) - BM25 + edge n-grams via Tantivy
2. **Semantic** - Vector similarity using real ML embeddings (MiniLM)
3. **Hybrid** - RRF fusion of lexical + semantic results

The implementation uses `fastembed` (ONNX, CPU-only) for embeddings. To preserve cass's existing privacy/UX contract ("no surprise network calls"), **model downloads are consent-gated**:

- By default, cass does **not** add new network calls.
- The semantic model is downloaded **only** after explicit user action (TUI prompt when switching to Semantic/Hybrid, or `cass models install`).
- Once installed, semantic search is fully offline.

Key improvements in this revision:
- Semantic/Hybrid respects existing filters (agent/workspace/source/time) and ranking modes.
- Vector index is compact + mmap-friendly (f16 default) to keep memory/disk low.
- Robust model pinning + verification (revision pin + checksums + atomic install).
- Better hybrid ranking quality (candidate depth, tie-break rules, optional diversity).

---

## Table of Contents

1. [Design Philosophy](#1-design-philosophy)
2. [Search Mode Architecture](#2-search-mode-architecture)
3. [Network Policy, Consent, and Model Management](#3-network-policy-consent-and-model-management)
4. [Embedding Strategy](#4-embedding-strategy)
5. [Vector Storage & Index](#5-vector-storage--index)
6. [Hybrid Search with RRF](#6-hybrid-search-with-rrf)
7. [TUI Integration](#7-tui-integration)
8. [CLI/Robot Mode Support](#8-clirobot-mode-support)
9. [Performance Considerations](#9-performance-considerations)
10. [Implementation Phases](#10-implementation-phases)
11. [File Structure](#11-file-structure)
12. [Dependencies](#12-dependencies)
13. [Testing Strategy](#13-testing-strategy)
14. [Open Questions](#14-open-questions)

---

## 1. Design Philosophy

### Core Principles

1. **Real Semantic by Default**: Uses actual ML embeddings (MiniLM) - not hash approximations
2. **No Surprise Network Calls**: Downloads happen only after explicit user opt-in (TUI prompt or CLI command)
3. **Zero-Drama Setup**: If the user opts in, download/install is automatic with progress + verification
4. **Fast Iteration**: Semantic search feels responsive (<100ms query time)
5. **Offline-First**: Once downloaded, no network required; everything runs locally
6. **Filter Parity**: Semantic/Hybrid must honor the same filters as Lexical (agent/workspace/source/time)
7. **Reproducible & Safe**: Pinned model revision + SHA256 verification + atomic installs; easy rollback

### Why Real Embeddings Over Hash?

The beads_viewer Go implementation currently uses only a hash-based embedder (FNV-1a feature hashing), with real sentence-transformers integration planned but not yet implemented. Their code explicitly notes the hash approach "is not a true 'semantic' model."

Since we're in Rust with access to `fastembed-rs` (pure Rust + ONNX, no Python), we can go directly to **real semantic embeddings** as the default. The `AllMiniLML6V2` model:
- Runs entirely on CPU (no GPU required)
- ~23MB download (one-time)
- ~15ms per embedding (fast enough for interactive use)
- Produces high-quality 384-dimensional semantic vectors

### User Experience Flow

```
First Run (model not installed):
┌─────────────────────────────────────────────────────────────────┐
│  cass starts → TUI loads immediately                            │
│  ↓                                                              │
│  User toggles Semantic/Hybrid (Alt+S)                           │
│  → Prompt: "Semantic requires 23MB model. [D]ownload / [H]ash / [Esc]"│
│  ↓                                                              │
│  User can keep searching (Lexical works); Semantic not available│
│  ↓                                                              │
│  User presses D → Download begins with progress                 │
│  Status bar: "⬇️ Downloading semantic model... 45%"              │
│  ↓                                                              │
│  Download completes → SHA256 verified → Model loaded            │
│  Toast: "✓ Semantic search ready"                               │
│  ↓                                                              │
│  Semantic/Hybrid modes now use real ML embeddings               │
└─────────────────────────────────────────────────────────────────┘

Subsequent Runs (model installed):
┌─────────────────────────────────────────────────────────────────┐
│  cass starts → Model already cached → Full semantic immediately │
└─────────────────────────────────────────────────────────────────┘

CLI Install (for automation / pre-provisioning):
┌─────────────────────────────────────────────────────────────────┐
│  cass models install → Downloads + verifies model               │
│  cass tui → Semantic ready immediately                          │
└─────────────────────────────────────────────────────────────────┘
```

---

## 2. Search Mode Architecture

### New SearchMode Enum

```rust
/// Search algorithm mode - cycles with Alt+S
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SearchMode {
    /// BM25 full-text search via Tantivy (current behavior)
    #[default]
    Lexical,
    /// Vector similarity search using embeddings
    Semantic,
    /// RRF fusion of lexical + semantic results
    Hybrid,
}

impl SearchMode {
    pub fn next(self) -> Self {
        match self {
            SearchMode::Lexical => SearchMode::Semantic,
            SearchMode::Semantic => SearchMode::Hybrid,
            SearchMode::Hybrid => SearchMode::Lexical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SearchMode::Lexical => "Lexical",
            SearchMode::Semantic => "Semantic",
            SearchMode::Hybrid => "Hybrid",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            SearchMode::Lexical => "LEX",
            SearchMode::Semantic => "SEM",
            SearchMode::Hybrid => "HYB",
        }
    }
}
```

### Integration with Existing Modes

The `SearchMode` is **orthogonal** to existing modes:
- `MatchMode` (Standard/Prefix) - affects lexical query construction
- `RankingMode` (Recent/Balanced/Relevance/etc.) - affects result ordering
- `SearchMode` (Lexical/Semantic/Hybrid) - affects which search algorithm(s) run

In **Semantic** and **Hybrid** modes, `RankingMode` must remain meaningful.

### RankingMode Behavior in Semantic

Semantic similarity uses cosine (dot product of L2-normalized vectors). We map to a non-negative range for blending:
`sim01 = clamp((sim + 1.0) / 2.0, 0.0..1.0)`

Then apply the same weighting patterns as Lexical:
- **Recent Heavy**: `score = sim01 * 0.3 + recency * 0.7`
- **Balanced**: `score = sim01 * 0.5 + recency * 0.5`
- **Relevance Heavy**: `score = sim01 * 0.8 + recency * 0.2`
- **Match Quality**: `score = sim01 * 0.85 + recency * 0.15` (semantic has no wildcard penalty)
- **Date Newest/Oldest**: ignore sim for ordering, but keep sim in metadata for inspection

### RankingMode Behavior in Hybrid

Hybrid ordering:
1. **Primary**: RRF rank fusion score
2. **Tie-break**: apply RankingMode-specific recency preference
3. **Tie-break**: higher `max(component_similarity, component_bm25)`

This makes HYB feel stable and consistent with the existing UX.

---

## 3. Network Policy, Consent, and Model Management

This feature introduces a new potential network call (model download). To preserve cass's current expectations:

- **Default behavior**: no new network calls.
- **Download trigger**: only after explicit user action:
  - TUI: switching to Semantic/Hybrid prompts and offers a single-key "Download" action.
  - CLI: `cass models install` or `cass index --semantic --download-model`.
- **Headless/CI**: if `TUI_HEADLESS=1` (or no TTY), cass never prompts; semantic remains unavailable unless the user ran an explicit install command.
- **Hard offline**: `CASS_OFFLINE=1` forbids downloads (and can also gate the existing update check).

### 3.1 New CLI Surface: `cass models`

```bash
# Show model status (installed, verified, revision, size)
cass models status [--json]

# Install the default model (consent explicitly given via command)
cass models install [--model all-minilm-l6-v2] [--mirror <url>] [--json]

# Verify checksums / repair if corrupted
cass models verify [--repair] [--json]

# Remove model files (reclaim disk)
cass models remove [--model all-minilm-l6-v2] [-y]
```

### 3.2 Consent Modes

`CASS_SEMANTIC_AUTODOWNLOAD` is tri-state:

```bash
# default: ask in TUI only, never in headless
CASS_SEMANTIC_AUTODOWNLOAD=ask

# always download when user switches to SEM/HYB in TUI (no prompt)
CASS_SEMANTIC_AUTODOWNLOAD=true

# never download (SEM/HYB requires manual `cass models install`)
CASS_SEMANTIC_AUTODOWNLOAD=false
```

### 3.3 Model State Machine

```rust
pub enum ModelState {
    /// Model not present on disk
    NotInstalled,
    /// SEM/HYB requested but user hasn't opted in yet
    NeedsConsent,
    /// Download in progress
    Downloading { progress_pct: u8, bytes_downloaded: u64, total_bytes: u64 },
    /// Download complete, verifying SHA256
    Verifying,
    /// Model ready to use
    Ready,
    /// Semantic disabled (offline mode, headless, or policy forbids)
    Disabled { reason: String },
    /// Verification failed, will retry or rebuild
    VerificationFailed { reason: String, retry_count: u8 },
}
```

### 3.4 Model Manifest (Reproducibility)

To make installs reproducible and verifiable, we pin a **Hugging Face revision** (commit hash):

```toml
# models.manifest.toml (checked into repo)
[[models]]
id = "all-minilm-l6-v2"
repo = "sentence-transformers/all-MiniLM-L6-v2"
revision = "e4ce9877abf3edfe10b0d82785e83bdcb973e22e"  # pinned commit
files = [
    { name = "model.onnx", sha256 = "abc123...", size = 22713856 },
    { name = "tokenizer.json", sha256 = "def456...", size = 711396 },
    { name = "config.json", sha256 = "789abc...", size = 612 },
]
license = "Apache-2.0"
attribution = "sentence-transformers/all-MiniLM-L6-v2 by UKPLab"
```

If Hugging Face is blocked, allow optional mirrors:
```bash
cass models install --mirror https://internal.mirror/models/
# or
CASS_SEMANTIC_MIRROR_URL=https://internal.mirror/models/
```

### 3.5 Model Selection

**Primary Model**: `sentence-transformers/all-MiniLM-L6-v2`
- **Dimension**: 384
- **Size**: ~23MB (ONNX)
- **Quality**: Excellent for code/technical content
- **Speed**: ~15ms per embedding on CPU
- **Source**: Hugging Face Hub (pinned revision, cached locally)

### 3.6 Download & Verification Flow

```rust
pub struct ModelManager {
    models_dir: PathBuf,
    manifest: ModelManifest,
    state: Arc<RwLock<ModelState>>,
    progress_tx: Option<mpsc::Sender<ModelProgress>>,
}

impl ModelManager {
    /// Check if model exists and is valid
    pub async fn check_model(&self) -> ModelState {
        let model_path = self.models_dir.join("all-MiniLM-L6-v2");

        if !model_path.exists() {
            return ModelState::NotInstalled;
        }

        // Verify all required files exist and checksums match
        for file_info in &self.manifest.files {
            let file_path = model_path.join(&file_info.name);
            if !file_path.exists() {
                return ModelState::NotInstalled;
            }

            let actual_hash = sha256_file(&file_path).await?;
            if actual_hash != file_info.sha256 {
                return ModelState::VerificationFailed {
                    reason: format!("{} checksum mismatch", file_info.name),
                    retry_count: 0,
                };
            }
        }

        ModelState::Ready
    }

    /// Download model with resumable downloads + atomic install
    pub async fn download_model(&self) -> Result<()> {
        let temp_dir = self.models_dir.join("all-MiniLM-L6-v2.downloading");
        let final_dir = self.models_dir.join("all-MiniLM-L6-v2");

        // Use resumable downloads (HTTP Range) + atomic install directory swap
        // Never leave partially-verified files in the active model dir.

        self.set_state(ModelState::Downloading {
            progress_pct: 0,
            bytes_downloaded: 0,
            total_bytes: self.manifest.total_size(),
        });

        for file_info in &self.manifest.files {
            let url = format!(
                "https://huggingface.co/{}/resolve/{}/{}",
                self.manifest.repo, self.manifest.revision, file_info.name
            );

            download_with_resume(&url, &temp_dir.join(&file_info.name), |progress| {
                self.report_progress(progress);
            }).await?;
        }

        // Verify all files
        self.set_state(ModelState::Verifying);
        for file_info in &self.manifest.files {
            let actual_hash = sha256_file(&temp_dir.join(&file_info.name)).await?;
            if actual_hash != file_info.sha256 {
                return Err(anyhow!("{} checksum mismatch", file_info.name));
            }
        }

        // Atomic swap: rename temp → final
        if final_dir.exists() {
            tokio::fs::rename(&final_dir, &self.models_dir.join("all-MiniLM-L6-v2.bak")).await?;
        }
        tokio::fs::rename(&temp_dir, &final_dir).await?;

        self.set_state(ModelState::Ready);
        Ok(())
    }
}
```

### 3.7 Background Download Integration

```rust
/// Spawned when SEM/HYB is requested and policy allows (or CLI install invoked)
pub async fn ensure_semantic_model(
    data_dir: &Path,
    progress_tx: mpsc::Sender<SemanticModelEvent>,
) {
    let manager = ModelManager::new(data_dir.join("models"));

    match manager.check_model().await {
        ModelState::Ready => {
            // Model already downloaded and verified
            let _ = progress_tx.send(SemanticModelEvent::Ready).await;
        }
        ModelState::NeedsConsent | ModelState::Disabled { .. } => {
            // User hasn't opted in or is in offline mode
            let _ = progress_tx.send(SemanticModelEvent::NeedsConsent).await;
        }
        ModelState::NotInstalled | ModelState::VerificationFailed { .. } => {
            // Start background download (consent already granted via CLI or TUI prompt)
            let _ = progress_tx.send(SemanticModelEvent::DownloadStarted).await;

            match manager.download_model().await {
                Ok(()) => {
                    let _ = progress_tx.send(SemanticModelEvent::Ready).await;
                }
                Err(e) => {
                    tracing::warn!("Model download failed: {}", e);
                    let _ = progress_tx.send(SemanticModelEvent::DownloadFailed {
                        reason: e.to_string(),
                    }).await;
                }
            }
        }
        _ => {}
    }
}

/// Events sent to TUI for status display
pub enum SemanticModelEvent {
    NeedsConsent,
    DownloadStarted,
    DownloadProgress { pct: u8 },
    Verifying,
    Ready,
    DownloadFailed { reason: String },
}
```

### 3.8 Graceful Fallback Options

If the ML model is not installed, cass can:

1. **Disable SEM/HYB** (default) - Keep Lexical working, show "model not installed" status
2. **Hash-only mode** (explicit opt-in) - Use hash embeddings as "approximate similarity"

Hash mode is **not marketed as true semantic** - it's labeled `SEM*` in the status bar and described as "approximate" in the help text.

```rust
pub struct SmartEmbedder {
    /// Real ML embedder (once loaded)
    ml_embedder: Option<Arc<FastEmbedder>>,
    /// Hash fallback (explicit opt-in only)
    hash_embedder: Option<HashEmbedder>,
    /// Current state
    state: Arc<RwLock<EmbedderState>>,
}

pub enum EmbedderState {
    /// ML model ready
    MlReady,
    /// ML not available, semantic disabled
    Unavailable,
    /// Hash-only mode (explicit opt-in via CASS_SEMANTIC_EMBEDDER=hash)
    HashFallback,
}

impl SmartEmbedder {
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match &*self.state.read() {
            EmbedderState::MlReady => {
                self.ml_embedder.as_ref().unwrap().embed(text)
            }
            EmbedderState::HashFallback => {
                self.hash_embedder.as_ref().unwrap().embed(text)
            }
            EmbedderState::Unavailable => {
                Err(anyhow!("Semantic search not available - model not installed"))
            }
        }
    }

    /// Check if we're using real semantic or hash approximation
    pub fn is_true_semantic(&self) -> bool {
        matches!(&*self.state.read(), EmbedderState::MlReady)
    }
}
```

Configuration:
```bash
# Force hash-only mode (labeled as "approximate", not true semantic)
CASS_SEMANTIC_EMBEDDER=hash
```

### 3.9 Index Upgrade Path

When the ML model becomes available, the vector index needs rebuilding:

```rust
/// Detect if vector index was built with hash vs ML embedder
pub fn index_needs_upgrade(index_path: &Path) -> bool {
    let metadata = VectorIndexMetadata::load(index_path);
    match metadata {
        Ok(meta) => meta.embedder_id.starts_with("hash-"),
        Err(_) => true, // No index, needs building
    }
}

/// Background task to upgrade index from hash to ML embeddings
pub async fn upgrade_vector_index(
    storage: &SqliteStorage,
    old_index: &VectorIndex,
    new_embedder: &FastEmbedder,
    progress_tx: mpsc::Sender<IndexProgress>,
) -> Result<VectorIndex> {
    let messages = storage.get_all_messages_for_embedding()?;
    let total = messages.len();

    let mut new_index = VectorIndex::new(new_embedder.dimension(), new_embedder.id());

    // Batch embed for efficiency
    for (i, batch) in messages.chunks(32).enumerate() {
        let texts: Vec<&str> = batch.iter().map(|m| m.content.as_str()).collect();
        let embeddings = new_embedder.embed_batch(&texts)?;

        for (msg, embedding) in batch.iter().zip(embeddings) {
            new_index.insert(
                &msg.source_path,
                msg.idx as u64,
                content_hash(&msg.content),
                embedding,
            );
        }

        let progress = ((i * 32) * 100 / total) as u8;
        let _ = progress_tx.send(IndexProgress::Semantic { pct: progress }).await;
    }

    new_index.save()?;
    Ok(new_index)
}
```

### 3.10 Storage Location

```
~/.local/share/coding-agent-search/
├── models/
│   └── all-MiniLM-L6-v2/
│       ├── model.onnx          # ~23MB ONNX weights
│       ├── tokenizer.json      # Tokenizer config
│       ├── config.json         # Model config
│       └── .verified           # Checksum verification marker
├── vector_index/
│   ├── index-hash-384.cvvi     # Hash-based index (fallback)
│   └── index-minilm-384.cvvi   # ML-based index (primary)
└── ...
```

### 3.11 Network Failure Handling

```rust
pub struct DownloadConfig {
    /// Maximum retries for failed downloads
    pub max_retries: u8,  // Default: 3
    /// Delay between retries (exponential backoff)
    pub retry_delay: Duration,  // Default: 5s, 15s, 45s
    /// Download timeout
    pub timeout: Duration,  // Default: 5 minutes
    /// Resume partial downloads
    pub resume_enabled: bool,  // Default: true
}

impl ModelManager {
    pub async fn download_with_retry(&self, config: &DownloadConfig) -> Result<()> {
        let mut attempts = 0;

        loop {
            match self.download_model().await {
                Ok(()) => return Ok(()),
                Err(e) if attempts < config.max_retries => {
                    attempts += 1;
                    let delay = config.retry_delay * (3_u32.pow(attempts as u32 - 1));
                    tracing::warn!(
                        "Download attempt {} failed: {}. Retrying in {:?}",
                        attempts, e, delay
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    tracing::error!("Download failed after {} attempts: {}", attempts, e);
                    return Err(e);
                }
            }
        }
    }
}
```

### 3.12 Offline Mode

For air-gapped environments, users can manually place model files:

```bash
# Manual model installation
mkdir -p ~/.local/share/coding-agent-search/models/all-MiniLM-L6-v2/
cp /path/to/model.onnx ~/.local/share/coding-agent-search/models/all-MiniLM-L6-v2/
cp /path/to/tokenizer.json ~/.local/share/coding-agent-search/models/all-MiniLM-L6-v2/
cp /path/to/config.json ~/.local/share/coding-agent-search/models/all-MiniLM-L6-v2/

# Verify installation
cass status --json | jq '.semantic_model'
# → { "state": "ready", "embedder": "minilm-384" }
```

Environment variable to disable auto-download:
```bash
export CASS_SEMANTIC_AUTODOWNLOAD=false
```

---

## 4. Embedding Strategy

### 4.0 What is Embedded (Scope)

To keep the index small and improve result quality, semantic embeddings are built for:
- **Roles**: `user` and `assistant` (default)
- **Excludes**: pure tool ack spam, empty messages, and optionally `system` (configurable)
- **Provenance fields** are stored alongside embeddings for fast filter parity:
  - `agent_slug` (or agent_id)
  - `workspace` (or workspace_id)
  - `source_id` (local/remote machine id; required for Remote Sources filtering)
  - `created_at` (ms)

Config knobs:
```bash
# Roles to include in semantic index
CASS_SEMANTIC_ROLES=user,assistant

# Include tool/system content (off by default)
CASS_SEMANTIC_INCLUDE_SYSTEM=false
CASS_SEMANTIC_INCLUDE_TOOLS=false
```

### 4.1 Embedder Trait

```rust
/// Trait for generating text embeddings
pub trait Embedder: Send + Sync {
    /// Generate embedding vector for text
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Batch embedding for efficiency
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embedding dimension
    fn dimension(&self) -> usize;

    /// Embedder identifier (for cache invalidation)
    fn id(&self) -> &str;

    /// Whether this embedder produces true semantic embeddings
    fn is_semantic(&self) -> bool;
}
```

### 4.2 FastEmbed Embedder (Primary - Real ML)

Using [fastembed-rs](https://github.com/Anush008/fastembed-rs) for ONNX-based inference:

```rust
use fastembed::{TextEmbedding, EmbeddingModel, InitOptions};

pub struct FastEmbedder {
    model: TextEmbedding,
    model_id: String,
    dimension: usize,
}

impl FastEmbedder {
    pub fn new(model_path: &Path) -> Result<Self> {
        let text_embedding = TextEmbedding::try_new(InitOptions {
            model_name: EmbeddingModel::AllMiniLML6V2,
            cache_dir: model_path.to_path_buf(),
            show_download_progress: false, // We handle progress ourselves
            ..Default::default()
        })?;

        Ok(Self {
            model: text_embedding,
            model_id: "minilm-384".to_string(),
            dimension: 384,
        })
    }
}

impl Embedder for FastEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let embeddings = self.model.embed(vec![text], None)?;
        Ok(embeddings.into_iter().next().unwrap())
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        Ok(self.model.embed(texts, None)?)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.model_id
    }

    fn is_semantic(&self) -> bool {
        true  // Real semantic understanding
    }
}
```

### 4.3 Hash Embedder (Fallback - During Download)

Based on beads_viewer's implementation, using **FNV-1a feature hashing**:

```rust
pub struct HashEmbedder {
    dimension: usize,  // Default: 384
}

impl HashEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    fn hash_token(token: &str) -> u64 {
        // FNV-1a hash
        let mut h: u64 = 0xcbf29ce484222325; // FNV offset basis
        for byte in token.as_bytes() {
            h ^= u64::from(*byte);
            h = h.wrapping_mul(0x100000001b3); // FNV prime
        }
        h
    }

    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty() && s.len() >= 2)
            .map(String::from)
            .collect()
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let tokens = Self::tokenize(text);
        let mut vec = vec![0.0f32; self.dimension];

        for token in tokens {
            let hash = Self::hash_token(&token);
            let idx = (hash % self.dimension as u64) as usize;
            let sign = if (hash >> 63) == 0 { 1.0 } else { -1.0 };
            vec[idx] += sign;
        }

        // L2 normalize
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }

        Ok(vec)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        "hash-384"
    }

    fn is_semantic(&self) -> bool {
        false  // Not true semantic - just keyword overlap
    }
}
```

**Why hash as fallback**: Hash embeddings provide a reasonable approximation while the ML model downloads:
- Instantaneous (no model loading)
- Deterministic (reproducible)
- Zero network dependency
- Better than no semantic search at all

**Important**: The TUI clearly indicates when using hash fallback vs real ML:
- Status bar shows `SEM*` (asterisk) when using hash fallback
- Shows `SEM` (no asterisk) when using real ML embeddings
- Toast notification when upgrade completes: "Semantic search upgraded to ML model"

### 4.4 Embedder Comparison

| Embedder | Type | Dimension | Speed | Quality | Use Case |
|----------|------|-----------|-------|---------|----------|
| **MiniLM (default)** | Real ML | 384 | ~15ms | Excellent | Primary - after download |
| **Hash (fallback)** | Approximate | 384 | <1ms | Fair | Temporary - during download |

**Alternative Models** (can be configured via env var):
| Model | Dimension | Size | Speed | Quality |
|-------|-----------|------|-------|---------|
| `AllMiniLML6V2` | 384 | ~23MB | Fast | Good |
| `AllMiniLML12V2` | 384 | ~33MB | Medium | Better |
| `BGESmallENV15` | 384 | ~33MB | Medium | Best (small) |
| `BGEBaseENV15` | 768 | ~110MB | Slower | Excellent |

### 4.5 Configuration

Environment variables for advanced users:
```bash
# Disable auto-download (use hash-only mode)
CASS_SEMANTIC_AUTODOWNLOAD=false

# Use a different model (requires manual download or will auto-download)
CASS_SEMANTIC_MODEL=BGESmallENV15

# Force hash-only mode (no ML)
CASS_SEMANTIC_EMBEDDER=hash
```

### 4.6 Canonicalization (Critical for Quality + Incremental Correctness)

Raw agent logs often include:
- Huge code blocks / diffs
- Tool call transcripts
- Repeated boilerplate / progress messages
- Markdown noise

We define a deterministic `canonicalize_for_embedding()` used for:
- ML embeddings
- Content hashing (so unchanged canonical text → unchanged hash)

**Proposed rules** (simple + fast, no ML):
1. Strip most markdown formatting (keep headings words, inline code text, link text)
2. Normalize whitespace (collapse runs, trim)
3. For fenced code blocks:
   - Keep first N lines + last N lines (defaults: 20/10)
   - Replace middle with `… [code omitted] …`
4. Drop known low-signal boilerplate ("Done.", "OK", empty tool acks)

```rust
pub fn canonicalize_for_embedding(raw: &str) -> String {
    let mut result = strip_markdown_formatting(raw);
    result = collapse_code_blocks(&result, 20, 10);
    result = normalize_whitespace(&result);
    result = filter_low_signal_content(&result);
    result.truncate(MAX_EMBED_CHARS);  // Default: 2000
    result
}

pub fn content_hash(raw: &str) -> [u8; 32] {
    let canonical = canonicalize_for_embedding(raw);
    sha256(canonical.as_bytes())
}
```

Config:
```bash
CASS_SEM_MAX_CHARS=2000
CASS_SEM_CODE_HEAD_LINES=20
CASS_SEM_CODE_TAIL_LINES=10
```

### 4.7 Chunking Long Messages (Optional, Bounded)

Some messages remain too large even after canonicalization. For those:
- Create up to **3** chunks (head / middle / tail) with distinct chunk_ids
- Each chunk becomes an embedding entry referencing the same message_id
- At render time, hits are collapsed to the message_id (best chunk score wins)

This improves recall for long "design doc" messages without exploding index size.

```rust
pub struct EmbeddingChunk {
    pub message_id: u64,
    pub chunk_idx: u8,      // 0, 1, or 2
    pub text: String,
    pub content_hash: [u8; 32],
}

pub fn chunk_for_embedding(message_id: u64, canonical: &str, max_chunk_chars: usize) -> Vec<EmbeddingChunk> {
    if canonical.len() <= max_chunk_chars {
        return vec![single_chunk(message_id, canonical)];
    }

    // Create head, middle (if distinct), tail chunks
    let head = &canonical[..max_chunk_chars];
    let tail = &canonical[canonical.len().saturating_sub(max_chunk_chars)..];

    let mut chunks = vec![
        EmbeddingChunk { message_id, chunk_idx: 0, text: head.to_string(), .. },
        EmbeddingChunk { message_id, chunk_idx: 2, text: tail.to_string(), .. },
    ];

    // Add middle chunk if content is long enough
    if canonical.len() > max_chunk_chars * 2 {
        let mid_start = canonical.len() / 2 - max_chunk_chars / 2;
        let middle = &canonical[mid_start..mid_start + max_chunk_chars];
        chunks.insert(1, EmbeddingChunk { message_id, chunk_idx: 1, text: middle.to_string(), .. });
    }

    chunks
}
```

---

## 5. Vector Storage & Index

### 5.1 Vector Index Structure

Binary format (`.cvvi` - Cass Vector Index), revised for compactness, mmap, and filter parity:

```
Header:
  Magic: "CVVI" (4 bytes)
  Version: u16 (little-endian)
  EmbedderID Length: u16
  EmbedderID: string (variable)
  Dimension: u32
  Quantization: u8              # 0=f32, 1=f16 (default)
  Count: u32
  HeaderCRC32: u32              # quick corruption detection

Rows (Count repeated, fixed-size):
  MessageID: u64                # stable SQLite message primary key
  CreatedAtMs: i64              # for recency/ranking + time filters
  AgentID: u32                  # small IDs for fast filtering
  WorkspaceID: u32
  SourceID: u32                 # remote provenance filter parity
  ChunkIdx: u8                  # 0 for single-chunk, 0-2 for multi-chunk
  VecOffset: u64                # offset into contiguous vector slab
  ContentHash: [32]u8           # SHA-256(canonical content)

Vectors slab:
  [quant; Count * Dimension]    # contiguous for fast streaming dot products
```

**Key design decisions**:
- **MessageID instead of (source_path, msg_idx)**: More stable across file moves, remote path mappings, and connector changes
- **Filter metadata inline**: Enables agent/workspace/source/time filtering without SQLite round-trips per candidate
- **Contiguous vector slab**: mmap-friendly, enables SIMD-optimized dot products

### 5.2 VectorIndex Implementation

```rust
pub struct VectorIndex {
    dimension: usize,
    embedder_id: String,
    quant: Quantization,               // f16 default, f32 optional
    rows: Vec<VectorRow>,              // fixed-size metadata per entry
    vectors: MmapVectors,              // contiguous vector storage (mmap-friendly)
}

pub struct VectorRow {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub chunk_idx: u8,
    pub vec_offset: u64,               // offset into contiguous vector slab
    pub content_hash: [u8; 32],
}

/// Filter for semantic search (mirrors SearchFilters)
pub struct SemanticFilter {
    pub agents: Option<HashSet<u32>>,
    pub workspaces: Option<HashSet<u32>>,
    pub sources: Option<HashSet<u32>>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
}

impl SemanticFilter {
    pub fn matches(&self, row: &VectorRow) -> bool {
        if let Some(agents) = &self.agents {
            if !agents.contains(&row.agent_id) { return false; }
        }
        if let Some(workspaces) = &self.workspaces {
            if !workspaces.contains(&row.workspace_id) { return false; }
        }
        if let Some(sources) = &self.sources {
            if !sources.contains(&row.source_id) { return false; }
        }
        if let Some(from) = self.created_from {
            if row.created_at_ms < from { return false; }
        }
        if let Some(to) = self.created_to {
            if row.created_at_ms > to { return false; }
        }
        true
    }
}

impl VectorIndex {
    /// Search for top-k most similar vectors with filter support
    pub fn search_top_k(&self, query_vec: &[f32], k: usize, filter: &SemanticFilter) -> Vec<VectorSearchResult> {
        let mut heap = BinaryHeap::with_capacity(k + 1);

        for row in &self.rows {
            // Apply filters inline - no DB round-trip needed
            if !filter.matches(row) { continue; }

            let vec = self.vectors.get(row.vec_offset, self.dimension, self.quant);
            let score = dot_product(query_vec, &vec);
            heap.push(Reverse(ScoredEntry {
                score,
                message_id: row.message_id,
                chunk_idx: row.chunk_idx,
            }));
            if heap.len() > k {
                heap.pop();
            }
        }

        // Collapse chunks to best-scoring chunk per message_id
        collapse_chunks(heap.into_sorted_vec())
    }
}

#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}
```

### 5.3 Atomic Writes and Recovery

Index writes must be crash-safe:

```rust
impl VectorIndex {
    pub fn save(&self, path: &Path) -> Result<()> {
        let temp_path = path.with_extension("cvvi.tmp");
        let backup_path = path.with_extension("cvvi.bak");

        // Write to temp file
        let mut file = File::create(&temp_path)?;
        self.write_to(&mut file)?;
        file.sync_all()?;  // fsync file

        // fsync directory for durability
        if let Some(parent) = temp_path.parent() {
            let dir = File::open(parent)?;
            dir.sync_all()?;
        }

        // Atomic rename: temp → final (keep backup)
        if path.exists() {
            std::fs::rename(path, &backup_path)?;
        }
        std::fs::rename(&temp_path, path)?;

        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let header = Self::read_header(&mut file)?;

        // Verify header + CRC
        if header.magic != *b"CVVI" {
            return Err(anyhow!("Invalid magic bytes"));
        }
        if header.crc32 != Self::compute_header_crc(&header) {
            return Err(anyhow!("Header CRC mismatch - index corrupted"));
        }

        // Verify file length is consistent with (count, dim, quant)
        let expected_size = Self::expected_file_size(&header);
        let actual_size = file.metadata()?.len();
        if actual_size != expected_size {
            return Err(anyhow!("File size mismatch - index corrupted"));
        }

        // Load rows + mmap vectors
        Self::load_from(&mut file, &header)
    }
}
```

**On corruption detection**:
- Mark semantic as temporarily unavailable
- Rebuild index in background (never crash the TUI)
- Log warning for user awareness

### 5.4 Quantization

Default **f16** halves disk + memory while keeping cosine similarity quality high for MiniLM-class models.

```rust
pub enum Quantization {
    F32,  // Full precision (4 bytes per component)
    F16,  // Half precision (2 bytes per component) - default
}

impl Quantization {
    pub fn bytes_per_component(&self) -> usize {
        match self {
            Quantization::F32 => 4,
            Quantization::F16 => 2,
        }
    }
}
```

**Memory comparison** (50k vectors × 384 dimensions):
- f32: 50,000 × 384 × 4 = **73.2 MB**
- f16: 50,000 × 384 × 2 = **36.6 MB**

Config:
```bash
# default
CASS_SEMANTIC_VECTOR_QUANT=f16

# for debugging / maximum precision
CASS_SEMANTIC_VECTOR_QUANT=f32
```

### 5.5 Content Hashing for Incremental Updates

Content hashing uses the **canonicalized** text (see Section 4.6):

```rust
fn content_hash(raw_content: &str) -> [u8; 32] {
    let canonical = canonicalize_for_embedding(raw_content);
    use ring::digest::{SHA256, digest};
    let result = digest(&SHA256, canonical.as_bytes());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(result.as_ref());
    hash
}
```

This allows efficient incremental indexing + filter parity:
- Skip unchanged messages (same content hash)
- Re-embed only modified content
- Track deletions by comparing indexed keys vs. current data
- Apply agent/workspace/source/time filters without DB round-tripping per candidate

### 5.6 Index File Location

```
~/.local/share/coding-agent-search/
├── agent_search.db           # SQLite (existing)
├── tantivy_index/            # Lexical index (existing)
└── vector_index/
    └── index-{embedder_id}.cvvi  # e.g., index-minilm-384.cvvi
```

---

## 6. Hybrid Search with RRF

### 6.1 Reciprocal Rank Fusion (RRF)

RRF is the industry-standard method for combining ranked result lists. It's simple, effective, and requires no tuning:

```rust
/// Reciprocal Rank Fusion constant (standard value)
const RRF_K: f32 = 60.0;

/// Calculate RRF score for a document at given rank
fn rrf_score(rank: usize) -> f32 {
    1.0 / (RRF_K + rank as f32 + 1.0)
}

/// Fuse two ranked result lists using RRF
pub fn rrf_fuse(
    lexical_results: &[SearchHit],
    semantic_results: &[VectorSearchResult],
    limit: usize,
) -> Vec<HybridSearchHit> {
    let mut scores: HashMap<(String, usize), HybridScore> = HashMap::new();

    // Add lexical scores
    for (rank, hit) in lexical_results.iter().enumerate() {
        let key = (hit.source_path.clone(), hit.line_number);
        let entry = scores.entry(key).or_default();
        entry.lexical_rank = Some(rank);
        entry.lexical_score = hit.score;
        entry.rrf_score += rrf_score(rank);
        entry.hit = Some(hit.clone());
    }

    // Add semantic scores
    for (rank, result) in semantic_results.iter().enumerate() {
        let key = (result.source_path.clone(), result.msg_idx as usize);
        let entry = scores.entry(key).or_default();
        entry.semantic_rank = Some(rank);
        entry.semantic_score = result.score;
        entry.rrf_score += rrf_score(rank);
    }

    // Sort by combined RRF score
    let mut results: Vec<_> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.rrf_score.partial_cmp(&a.1.rrf_score).unwrap());

    results.into_iter()
        .take(limit)
        .filter_map(|(_, score)| score.into_hybrid_hit())
        .collect()
}
```

### 6.2 Why RRF?

From research and industry practice:
- **No score normalization needed**: Lexical (BM25) and semantic (cosine) scores are on different scales; RRF uses ranks, not scores
- **Robust**: Works well across different query types without tuning
- **Simple**: One parameter (k=60) that rarely needs adjustment
- **Proven**: Used by Elasticsearch, OpenSearch, Qdrant, Milvus, Azure AI Search

### 6.3 Candidate Depth for Hybrid

When running hybrid search, we fetch more candidates than the final result limit:

```rust
pub struct HybridConfig {
    /// How many lexical candidates to fetch (multiplier of final limit)
    pub lexical_depth_multiplier: usize,  // Default: 3
    /// How many semantic candidates to fetch (multiplier of final limit)
    pub semantic_depth_multiplier: usize, // Default: 3
}

impl HybridConfig {
    pub fn candidate_counts(&self, final_limit: usize) -> (usize, usize) {
        (
            final_limit * self.lexical_depth_multiplier,
            final_limit * self.semantic_depth_multiplier,
        )
    }
}
```

**Rationale**: RRF fusion works best when both sources contribute meaningful candidates. If we only fetch `limit` candidates from each, we may miss good fusion opportunities. Fetching 3× from each gives room for rank-based scoring to work properly.

### 6.4 Extended Hybrid Scoring (Optional)

For more sophisticated ranking, we can add additional signals like beads_viewer:

```rust
pub struct HybridWeights {
    pub text_relevance: f32,    // Default: 0.40
    pub semantic_similarity: f32, // Default: 0.30
    pub recency: f32,           // Default: 0.20
    pub source_diversity: f32,  // Default: 0.10
}

impl Default for HybridWeights {
    fn default() -> Self {
        Self {
            text_relevance: 0.40,
            semantic_similarity: 0.30,
            recency: 0.20,
            source_diversity: 0.10,
        }
    }
}
```

**Weight presets** (like beads_viewer):
| Preset | Text | Semantic | Recency | Diversity |
|--------|------|----------|---------|-----------|
| `balanced` | 0.35 | 0.35 | 0.20 | 0.10 |
| `semantic-heavy` | 0.20 | 0.50 | 0.20 | 0.10 |
| `recent-first` | 0.30 | 0.20 | 0.40 | 0.10 |

### 6.5 Diversity (Optional Enhancement)

When top results cluster around the same agent/session, users may want source variety. An optional diversity penalty demotes consecutive same-source results:

```rust
pub fn apply_diversity_penalty(
    results: &mut Vec<HybridSearchHit>,
    penalty_factor: f32,  // Default: 0.1
    window_size: usize,   // Default: 3
) {
    let mut seen_sources: VecDeque<u32> = VecDeque::with_capacity(window_size);

    for result in results.iter_mut() {
        let source_id = result.source_id;

        // Count how many times this source appears in recent window
        let repeat_count = seen_sources.iter().filter(|&&s| s == source_id).count();

        if repeat_count > 0 {
            // Apply cumulative penalty for repeats
            result.rrf_score *= 1.0 - (penalty_factor * repeat_count as f32);
        }

        // Update sliding window
        if seen_sources.len() >= window_size {
            seen_sources.pop_front();
        }
        seen_sources.push_back(source_id);
    }

    // Re-sort after penalties
    results.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap());
}
```

Config:
```bash
# Enable diversity penalty (default: off)
CASS_HYBRID_DIVERSITY=true
CASS_HYBRID_DIVERSITY_PENALTY=0.1
CASS_HYBRID_DIVERSITY_WINDOW=3
```

---

## 7. TUI Integration

### 7.1 Keyboard Shortcut

**Recommended: `Alt+S`** (mnemonic: **S**earch mode)

Rationale:
- `Ctrl+S` is universally "save" - would confuse users
- `F9` is already taken by MatchMode
- `F12` is already taken by RankingMode
- `Alt+S` is unused and memorable

Alternative: `Ctrl+/` (search options)

### 7.2 TUI State Machine for SearchMode

The TUI tracks semantic availability explicitly:

```rust
pub enum SemanticAvailability {
    /// Model not installed, semantic disabled
    NotInstalled,
    /// User hasn't opted in yet (shown on first Alt+S to SEM/HYB)
    NeedsConsent,
    /// Download in progress
    Downloading { progress_pct: u8 },
    /// Model ready, semantic available
    Ready,
    /// Hash-only fallback (explicit opt-in via CASS_SEMANTIC_EMBEDDER=hash)
    HashFallback,
    /// Offline mode or policy disabled semantic
    Disabled { reason: String },
}

impl TuiState {
    pub fn handle_search_mode_toggle(&mut self) {
        let target_mode = self.search_mode.next();

        match target_mode {
            SearchMode::Lexical => {
                // Always allowed
                self.search_mode = target_mode;
            }
            SearchMode::Semantic | SearchMode::Hybrid => {
                match self.semantic_availability {
                    SemanticAvailability::Ready | SemanticAvailability::HashFallback => {
                        self.search_mode = target_mode;
                    }
                    SemanticAvailability::NotInstalled | SemanticAvailability::NeedsConsent => {
                        // Show install prompt
                        self.show_semantic_install_prompt = true;
                    }
                    SemanticAvailability::Downloading { .. } => {
                        // Show "downloading..." toast, stay on current mode
                        self.show_toast("Semantic model downloading...", ToastLevel::Info);
                    }
                    SemanticAvailability::Disabled { ref reason } => {
                        self.show_toast(&format!("Semantic disabled: {}", reason), ToastLevel::Warn);
                    }
                }
            }
        }
    }
}
```

### 7.3 Install Prompt Dialog

When user presses Alt+S to switch to SEM/HYB and model is not installed:

```
┌─────────────────────────────────────────────────────────────┐
│  Semantic Search                                            │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Semantic search requires a 23MB model download.            │
│                                                             │
│  The model (MiniLM-L6-v2) runs locally after download.      │
│  No data is sent to external services.                      │
│                                                             │
│  [D] Download now   [H] Use hash (approximate)   [Esc] Cancel│
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

**Key handling**:
- `D` → Start download, show progress in status bar, switch to SEM/HYB when ready
- `H` → Enable hash fallback mode (`SEM*`), switch to SEM/HYB immediately
- `Esc` → Cancel, stay on current mode

### 7.4 Status Bar Display

Current footer format:
```
[query: auth*] [agent: claude] rank:relevance ctx:M
```

New format with search mode:
```
[query: auth*] [agent: claude] mode:HYB rank:relevance ctx:M
```

Or with color coding:
- **LEX** - default text color (current behavior)
- **SEM** - cyan/blue (indicates ML vector search active)
- **SEM*** - cyan/blue with asterisk (indicates hash fallback mode)
- **HYB** - magenta/purple (indicates fusion)

### 7.5 Mode Indicator in Breadcrumbs

```rust
// In src/ui/components/breadcrumbs.rs
fn search_mode_label(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Lexical => "Lexical",
        SearchMode::Semantic => "Semantic",
        SearchMode::Hybrid => "Hybrid (RRF)",
    }
}
```

### 7.6 Help Screen Updates

Add to F1 help overlay:
```
SEARCH MODES
  Alt+S      Cycle search mode (Lexical → Semantic → Hybrid)

  Lexical    BM25 full-text search (fast, keyword-focused)
  Semantic   Vector similarity search (meaning-focused)
  Hybrid     RRF fusion of both (best of both worlds)
```

### 7.7 First-Time User Hint

When user first switches to Semantic mode with hash embedder:
```
Tip: Using lightweight hash embeddings. For better semantic search,
set CASS_SEMANTIC_EMBEDDER=fastembed (requires ~23MB model download).
```

This appears as a transient toast notification.

### 7.8 Visual Feedback During Embedding

During initial vector index build:
```
📦 Building semantic index... 150/2000 (7%) ▁▂▄▆█
```

Same sparkline visualization as current lexical indexing.

---

## 8. CLI/Robot Mode Support

### 8.1 New CLI Flags

```bash
# Search mode selection
cass search "query" --mode lexical|semantic|hybrid

# Explicit embedder override (for testing/comparison)
cass search "query" --embedder hash|fastembed

# Robot output includes mode info
cass search "query" --robot --mode hybrid

# Force semantic index rebuild
cass index --semantic --force
```

### 8.2 Robot Output Schema

```json
{
  "hits": [
    {
      "source_path": "/path/to/session.jsonl",
      "message_id": 12345,
      "msg_idx": 42,
      "agent": "claude-code",
      "workspace": "/Users/dev/project",
      "role": "assistant",
      "content_preview": "Let me help you with...",
      "created_at": "2025-12-18T10:30:00Z",
      "scores": {
        "lexical_rank": 3,
        "semantic_rank": 1,
        "rrf_score": 0.0328,
        "lexical_bm25": 12.5,
        "semantic_similarity": 0.89
      }
    }
  ],
  "_meta": {
    "query": "authentication flow",
    "elapsed_ms": 45,
    "search_mode": "hybrid",
    "embedder": "minilm-384",
    "embedder_is_semantic": true,
    "lexical_candidates": 150,
    "semantic_candidates": 150,
    "fused_results": 50,
    "rrf_k": 60,
    "filters_applied": {
      "agents": ["claude-code"],
      "workspaces": null,
      "sources": null,
      "time_range": null
    }
  }
}
```

### 8.3 Index Commands

```bash
# Build/rebuild vector index
cass index --semantic [--embedder hash|fastembed]

# Full rebuild including semantic
cass index --full --semantic

# Status includes vector index info
cass status --json
# → { "vector_index": { "embedder": "hash-384", "entries": 5000, "stale": false } }
```

### 8.4 Capabilities Update

```json
{
  "features": [
    "semantic_search",
    "hybrid_search",
    "rrf_fusion",
    "model_auto_download",
    "consent_gated_download",
    ...
  ],
  "embedders": {
    "available": ["hash", "minilm"],
    "active": "minilm",
    "is_semantic": true
  },
  "semantic_model": {
    "state": "ready",
    "model_id": "all-minilm-l6-v2",
    "dimension": 384,
    "download_size_bytes": 23000000
  }
}
```

---

## 9. Performance Considerations

### 9.1 Embedding Latency

| Embedder | Single Text | Batch (100) | Memory |
|----------|-------------|-------------|--------|
| Hash | <1ms | <10ms | ~1MB |
| AllMiniLML6V2 | ~15ms | ~200ms | ~100MB |
| BGESmallENV15 | ~20ms | ~300ms | ~150MB |

**Target**: Query-time embedding should complete in <50ms for interactive feel.

### 9.2 Vector Search Latency

With brute-force dot product (no ANN):
| Corpus Size | Search Time |
|-------------|-------------|
| 10K vectors | ~2ms |
| 50K vectors | ~10ms |
| 100K vectors | ~20ms |
| 500K vectors | ~100ms |

For typical cass usage (10K-50K messages), brute-force is fast enough.

### 9.3 When to Use HNSW

If corpus grows beyond 100K messages, consider adding HNSW (Hierarchical Navigable Small World) index:
- `hnsw` crate in Rust
- O(log n) search instead of O(n)
- Tradeoff: Index build time, memory overhead, approximate results

**Decision**: Start with brute-force; add HNSW as opt-in for large corpora.

### 9.4 Caching Strategy

```rust
pub struct SemanticCache {
    query_cache: LruCache<String, Vec<f32>>,  // Query text -> embedding
    result_cache: LruCache<(String, SearchMode), Vec<SearchHit>>, // (query, mode) -> results
}
```

Cache query embeddings since the same query is often run multiple times during a session.

### 9.5 Async Index Building

Like beads_viewer, build semantic index asynchronously:
1. TUI starts immediately with lexical-only search
2. Background task builds/updates vector index
3. Once ready, semantic/hybrid modes become available
4. Toast notification: "Semantic search ready"

---

## 10. Implementation Phases

### Phase 1: Foundation & Model Management
**Core infrastructure for semantic search**

- [ ] Create `src/search/embedder.rs` with `Embedder` trait
- [ ] Implement `HashEmbedder` (fallback)
- [ ] Implement `FastEmbedder` (primary ML embedder)
- [ ] Create `src/search/model_manager.rs` for auto-download
- [ ] Add SHA256 verification for model files
- [ ] Background download with progress reporting
- [ ] Graceful fallback to hash during download

### Phase 2: Vector Index & Storage
**Persistent vector storage**

- [ ] Create `src/search/vector_index.rs` with `.cvvi` binary format
- [ ] Implement vector index save/load
- [ ] Content hashing for incremental updates
- [ ] Index upgrade path (hash → ML)
- [ ] Wire into indexer for automatic building

### Phase 3: Semantic Search Integration
**Vector similarity search**

- [ ] Add `search_semantic()` to `SearchClient`
- [ ] Implement dot-product similarity search
- [ ] Integrate with TUI search flow
- [ ] Add `SearchMode` enum (Lexical/Semantic/Hybrid)
- [ ] Progress reporting for index building

### Phase 4: Hybrid Search with RRF
**Fusion of lexical + semantic**

- [ ] Implement `rrf_fuse()` function
- [ ] Add `search_hybrid()` to `SearchClient`
- [ ] Create `HybridSearchHit` with component scores
- [ ] Wire up hybrid mode in TUI
- [ ] Status bar shows mode indicator

### Phase 5: TUI Polish
**User experience refinements**

- [ ] Wire up `Alt+S` keyboard shortcut to cycle modes
- [ ] Status bar: `LEX` / `SEM` / `SEM*` (fallback) / `HYB`
- [ ] Download progress in status bar during first run
- [ ] Toast notifications for state changes
- [ ] Help screen updates (F1)
- [ ] Persist search mode preference

### Phase 6: CLI & Robot Mode
**Command-line interface support**

- [ ] Add `--mode` flag to search command
- [ ] Add `--semantic` flag to index command
- [ ] Update robot output schema with mode info
- [ ] Update `capabilities` command
- [ ] Add `cass status --json` semantic model info
- [ ] Write tests

### Phase 7: Advanced Features (Future)
**Deferred to later iterations**

- [ ] HNSW index for large corpora (>100K messages)
- [ ] Hybrid weight presets (balanced, semantic-heavy, etc.)
- [ ] API-based embedders (OpenAI, Cohere) for cloud option
- [ ] Query expansion using embeddings
- [ ] Semantic similarity "more like this" feature

---

## 11. File Structure

```
src/
├── search/
│   ├── mod.rs              # Add embedder + vector_index + model_manager modules
│   ├── query.rs            # Add SearchMode, hybrid search methods
│   ├── tantivy.rs          # Existing (unchanged)
│   ├── embedder.rs         # NEW: Embedder trait + HashEmbedder
│   ├── fastembed.rs        # NEW: FastEmbedder (MiniLM integration)
│   ├── model_manager.rs    # NEW: Auto-download, SHA256 verify, state machine
│   ├── vector_index.rs     # NEW: VectorIndex + .cvvi binary format
│   └── rrf.rs              # NEW: Reciprocal Rank Fusion implementation
├── indexer/
│   └── mod.rs              # Add semantic index building + upgrade path
├── ui/
│   ├── tui.rs              # Add SearchMode state, Alt+S handler, download progress
│   └── components/
│       └── breadcrumbs.rs  # Add search mode display (LEX/SEM/SEM*/HYB)
└── lib.rs                  # Add --mode CLI flag

Data directory (~/.local/share/coding-agent-search/):
├── models/
│   └── all-MiniLM-L6-v2/   # Auto-downloaded ML model
│       ├── model.onnx
│       ├── tokenizer.json
│       ├── config.json
│       └── .verified       # Checksum verification marker
├── vector_index/
│   └── index-minilm-384.cvvi  # Semantic vector index
└── ... (existing files)
```

---

## 12. Dependencies

### New Dependencies
```toml
[dependencies]
# Semantic search embeddings (ONNX-based, CPU inference)
fastembed = "4"

# f16 quantization for compact vector storage
half = "2"

# Already present - reused for checksums
ring = "0.17"  # SHA-256 for model verification

# Already present - reused for async download
reqwest = { version = "*", features = ["stream"] }  # Add "stream" feature
tokio = { version = "*", features = ["fs"] }  # Already present

# Memory-mapped file support (for large vector indices)
memmap2 = "0.9"
```

### Feature Flags (Optional)
```toml
[features]
default = ["semantic"]
semantic = ["fastembed", "half", "memmap2"]
```

This allows building without semantic search for minimal binary size:
```bash
cargo build --release --no-default-features
```

### Binary Size Impact
- **Without semantic**: Current binary size
- **With fastembed**: +5-10MB (ONNX runtime bindings)
- **With half + memmap2**: +~100KB

The model files (~23MB) are downloaded separately and cached in the data directory, not bundled in the binary.

---

## 13. Testing Strategy

### Unit Tests

**Embedder tests**:
- `test_hash_embedder_deterministic` - Same input → same output
- `test_hash_embedder_dimension` - Output is correct size
- `test_hash_embedder_normalized` - L2 norm = 1.0
- `test_fastembed_loads_model` - Model loads successfully from cache
- `test_embedder_trait_consistency` - Hash and ML embedders have same interface

**Vector index tests**:
- `test_vector_index_roundtrip` - Save/load preserves data
- `test_vector_index_atomic_write` - Crash mid-write doesn't corrupt
- `test_vector_index_crc_validation` - Detects corrupted headers
- `test_vector_index_f16_quantization` - f16 produces equivalent rankings to f32
- `test_vector_index_filter_parity` - Filters work correctly (agent/workspace/source/time)

**RRF fusion tests**:
- `test_rrf_fusion_ordering` - Top results are correct
- `test_rrf_handles_disjoint_sets` - Works when lists don't overlap
- `test_rrf_tie_breaking` - Consistent tie-break behavior
- `test_rrf_candidate_depth` - More candidates improves fusion quality

**Canonicalization tests**:
- `test_canonicalize_strips_markdown` - Removes formatting
- `test_canonicalize_collapses_code` - Long code blocks truncated
- `test_canonicalize_deterministic` - Same input → same output
- `test_content_hash_stability` - Hash is stable across runs

**Model management tests**:
- `test_model_state_transitions` - NotInstalled → Downloading → Ready
- `test_model_verification_catches_corruption` - Bad checksum detected
- `test_model_atomic_install` - Partial download doesn't leave broken state
- `test_consent_gated_download` - No network without explicit opt-in

### Integration Tests
- `test_semantic_search_returns_results` - Basic semantic search works
- `test_hybrid_search_improves_recall` - Hybrid finds more relevant results
- `test_incremental_index_skips_unchanged` - Only new messages embedded
- `test_search_mode_persists_in_tui_state` - Mode survives restart
- `test_filter_parity_semantic_vs_lexical` - Same filters produce consistent results
- `test_tui_install_prompt_shown` - Prompt appears on first SEM/HYB toggle
- `test_offline_mode_disables_download` - CASS_OFFLINE=1 prevents network

### CLI/Robot Mode Tests
- `test_robot_output_schema` - JSON output matches schema
- `test_robot_mode_hybrid_search` - `--mode hybrid` works in robot mode
- `test_cass_models_status` - `cass models status` returns correct info
- `test_cass_models_install` - `cass models install` downloads model

### Benchmark Tests
```rust
#[bench]
fn bench_hash_embed_1000_docs(b: &mut Bencher) { ... }

#[bench]
fn bench_fastembed_embed_100_docs(b: &mut Bencher) { ... }

#[bench]
fn bench_vector_search_10k(b: &mut Bencher) { ... }

#[bench]
fn bench_vector_search_50k_filtered(b: &mut Bencher) { ... }

#[bench]
fn bench_rrf_fusion_100_results(b: &mut Bencher) { ... }

#[bench]
fn bench_canonicalize_long_message(b: &mut Bencher) { ... }
```

---

## 14. Open Questions

### Q1: What if the model download fails repeatedly?

**Decided**: After 3 retries with exponential backoff, fall back to hash-only mode with a clear status indicator (`SEM*`) and periodic retry attempts in the background.

**Open**: Should we show a more prominent warning after N failed attempts? Or just silently continue with hash fallback?

### Q2: Should we pre-download the model on `cass index`?

**Option A**: Only download when TUI starts (current plan)
- Pro: CLI-only users don't download unnecessarily
- Con: First TUI launch has download delay

**Option B**: Download during `cass index --full`
- Pro: Everything ready when TUI opens
- Con: Slower initial index, larger scope for index command

**Leaning**: Option A, but could add `cass index --download-model` for users who want to pre-download.

### Q3: RRF constant (k) value?

**Standard**: k=60 is the industry default (Elasticsearch, Qdrant, etc.)

**Open**: Should we expose this as a tunable parameter for power users? Or keep it fixed at 60?

**Leaning**: Fixed at 60 initially; add `CASS_RRF_K` env var later if there's demand.

### Q4: Index upgrade notification?

When ML model becomes available and the vector index needs rebuilding from hash to ML:

**Option A**: Silently rebuild in background
**Option B**: Show toast: "Upgrading semantic index..."
**Option C**: Prompt user: "ML model ready. Rebuild index now? [Y/n]"

**Leaning**: Option B - informative but non-blocking.

### Q5: Multiple model support?

Should users be able to switch between different models (e.g., MiniLM vs BGE)?

**Current plan**: Single model (MiniLM) for simplicity. Different models have different embedding dimensions, which would require separate vector indices.

**Future**: Could support model selection via env var, with automatic index rebuild when model changes.

---

## Summary

This plan adds **real semantic search** and **hybrid search with RRF reranking** to cass with:

1. **Auto-download ML model** on first run (~23MB MiniLM, zero-config)
2. **Graceful degradation** to hash embeddings while model downloads
3. **Seamless upgrade** when model becomes available
4. **RRF fusion** for hybrid search (industry-standard k=60)
5. **Alt+S keyboard shortcut** to cycle modes (LEX → SEM → HYB)
6. **Clear status indicators** (`SEM` vs `SEM*` for fallback mode)
7. **Incremental indexing** with content hashing
8. **Full CLI/robot support** for automation

### Key Differences from beads_viewer

| Aspect | beads_viewer (Go) | cass (Rust) |
|--------|-------------------|-------------|
| Default embedder | Hash (only impl) | MiniLM ML (auto-download) |
| ML support | Planned, not implemented | Built-in via fastembed |
| Python dependency | Planned for sentence-transformers | None (pure Rust + ONNX) |
| Model management | Manual | Automatic with verification |

The implementation goes beyond beads_viewer's current capabilities by shipping with **real semantic embeddings out of the box**, while maintaining the same graceful degradation pattern for offline/constrained environments.

---

## References

- [fastembed-rs](https://github.com/Anush008/fastembed-rs) - Rust embedding library (ONNX-based)
- [ort](https://github.com/pykeio/ort) - ONNX Runtime for Rust
- [all-MiniLM-L6-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) - Default embedding model
- [Reciprocal Rank Fusion](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/reciprocal-rank-fusion) - Elasticsearch docs
- [Qdrant Hybrid Queries](https://qdrant.tech/documentation/concepts/hybrid-queries/) - RRF implementation
- [beads_viewer](https://github.com/dicklesworthstone/beads) - Reference implementation (Go, hash-only currently)
