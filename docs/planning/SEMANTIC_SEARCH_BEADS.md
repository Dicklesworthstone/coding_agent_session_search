# Semantic Search Implementation: Bead Structure & Elaboration

## Overview

This document elaborates on the semantic search plan and defines the complete bead (task) hierarchy for implementation. It's designed to be self-contained so any developer can pick up the work.

## Design Review: Optimizations Applied

### Critical Fixes Applied During Review

| Issue | Bead | Fix Applied |
|-------|------|-------------|
| Missing Unicode normalization | `8q8f` | Added NFC normalization as first step in canonicalization |
| Missing role field for filter parity | `cyra`, `vwxq` | Added `Role: u8` to VectorRow for user/assistant filtering |
| Missing IndexBuilding state | `vh6q` | Added state for when model is ready but index is being built |
| Misleading dialog text | `44pw` | Fixed text to accurately describe HuggingFace download |
| Non-deterministic RRF | `rzrv` | Added explicit tie-breaking rules for reproducible results |
| No SIMD optimization note | `tn4t` | Added requirement for SIMD-friendly patterns and alignment |
| No model upgrade path | `94pe` | Added version detection and index migration on model change |
| No offline install option | `3e28` | Added `--from-file` option for air-gapped environments |
| Missing determinism tests | `3qvr`, `c8f8` | Added Unicode and tie-breaking determinism tests |

---

### 1. Hash Fallback Strategy (Validated)
**Decision**: ML embeddings as primary, hash as explicit fallback only.

**Why not ship hash-first?** Hash "semantic" search is misleading - it's really just keyword overlap with different scoring. Users would form a negative impression. Better to gate behind consent and deliver the real thing.

**Fallback use case**: `CASS_SEMANTIC_EMBEDDER=hash` for air-gapped environments or users who want instant results without download.

### 2. Vector Index Format (Validated)
**Decision**: Custom `.cvvi` binary format rather than SQLite virtual table or Arrow.

**Why?** Our use case is narrow: mmap a contiguous vector array, scan with dot products, filter by inline metadata. SQLite's rowid joins would be slower. Arrow adds 5MB+ dependency for features we don't need.

**Format is right-sized**: Header with CRC32, fixed-size rows with filter metadata, contiguous f16 vector slab.

### 3. Inline Filter Metadata (Critical)
**Decision**: Store `agent_id`, `workspace_id`, `source_id`, `created_at_ms` per vector row.

**Why this matters**: Without inline metadata, semantic search requires DB joins per candidate. For 50k vectors, that's 50k SQLite lookups vs. inline integer comparisons. ~100x faster.

**Space cost**: ~24 bytes per row × 50k = 1.2MB. Worth it.

### 4. Chunking Strategy (Simplified)
**Original**: Head/middle/tail chunking for long messages.

**Optimization**: Make chunking optional and simple. Most agent messages are <2000 chars. Only 5-10% need chunking. Default: single chunk, truncated at 2000 chars canonical. Optional: enable multi-chunk for large corpus users.

**Why simplify?** Chunking adds complexity (chunk deduplication, score aggregation, UI for chunk navigation). Ship without it first, add based on user feedback.

### 5. Consent Flow (Validated)
**Decision**: TUI prompt on first Alt+S to SEM/HYB when model not installed.

**Why this is optimal**:
- Non-blocking: prompt only appears when user actually wants semantic
- Single-keypress action: D to download, H for hash, Esc to cancel
- Respects user agency: no surprise downloads
- Remembers choice: once downloaded, never prompts again

### 6. Diversity Penalty (Deferred)
**Original**: Optional diversity penalty to demote same-source clusters.

**Optimization**: Remove from initial implementation. RRF already provides some diversity naturally. Add later if users report clustering issues.

**Why defer?** It's a tuning knob that most users won't understand. Better to ship clean RRF and add diversity as a power-user option.

### 7. Query Cache (Essential)
**Decision**: Include LRU cache for query embeddings.

**Why essential**: Query embedding takes ~15ms. Users often re-run same query (typo fix, mode change). Cache hit = 0ms instead of 15ms. Significant UX improvement.

---

## Dependency Graph

```
                    ┌─────────────────┐
                    │  sem.emb.trait  │ Layer 0: Foundation
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
              ▼              ▼              ▼
     ┌────────────┐  ┌─────────────┐  ┌────────────┐
     │sem.emb.hash│  │sem.emb.canon│  │sem.vec.fmt │ Layer 1: Core
     └──────┬─────┘  └──────┬──────┘  └─────┬──────┘
            │               │               │
            │               ▼               │
            │        ┌─────────────┐        │
            │        │sem.emb.ml   │        │
            │        └──────┬──────┘        │
            │               │               │
            ▼               │               ▼
     ┌──────────────────────┴───────────────────────┐
     │                  sem.vec.ops                  │ Layer 2: Storage
     └─────────────────────┬────────────────────────┘
                           │
              ┌────────────┴────────────┐
              │                         │
              ▼                         ▼
     ┌────────────────┐        ┌───────────────┐
     │  sem.vec.filt  │        │ sem.mod.core  │ Layer 3: Features
     └───────┬────────┘        └───────┬───────┘
             │                         │
             └────────────┬────────────┘
                          │
                          ▼
              ┌───────────────────────┐
              │     hyb.search        │ Layer 4: Search
              └───────────┬───────────┘
                          │
              ┌───────────┼───────────┐
              │           │           │
              ▼           ▼           ▼
        ┌─────────┐ ┌─────────┐ ┌─────────┐
        │ hyb.rrf │ │hyb.rank │ │hyb.filt │ Layer 5: Hybrid
        └────┬────┘ └────┬────┘ └────┬────┘
             │           │           │
             └───────────┴───────────┘
                         │
         ┌───────────────┼───────────────┐
         │               │               │
         ▼               ▼               ▼
   ┌───────────┐  ┌────────────┐  ┌────────────┐
   │tui.sem.*  │  │cli.models  │  │cli.search  │ Layer 6: Interface
   └───────────┘  └────────────┘  └────────────┘
                         │
                         ▼
              ┌───────────────────────┐
              │      tst.sem.*        │ Layer 7: Testing
              └───────────────────────┘
```

---

## Bead Definitions

### Layer 0: Foundation

#### sem.emb.trait
**Type**: task | **Priority**: P1 (high)

**Purpose**: Define the Embedder trait that all embedding implementations must satisfy.

**Background**: The trait abstraction allows us to swap embedders (hash vs ML) transparently. This is critical for the consent-gated download flow where we start with hash and upgrade to ML.

**Deliverables**:
- `src/search/embedder.rs` with `Embedder` trait
- `embed(&self, text: &str) -> Result<Vec<f32>>`
- `embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>`
- `dimension(&self) -> usize`
- `id(&self) -> &str` (for cache invalidation)
- `is_semantic(&self) -> bool` (true=ML, false=hash)

**Acceptance criteria**:
- Trait compiles and is exported from `search` module
- Documentation explains each method's contract
- No external dependencies (pure trait definition)

---

### Layer 1: Core Components

#### sem.emb.hash
**Type**: task | **Priority**: P1 | **Depends on**: sem.emb.trait

**Purpose**: Implement FNV-1a feature hashing embedder as deterministic fallback.

**Background**: Hash embeddings are not "true" semantic (they're keyword overlap with random projection). But they're:
- Instant (no model loading)
- Deterministic (reproducible)
- Zero network dependency
Used when: (a) ML model not installed, (b) user explicitly opts for hash mode.

**Key implementation details**:
```rust
// FNV-1a hash for tokens
fn hash_token(token: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in token.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
```
- Tokenization: lowercase, split on non-alphanumeric, filter len >= 2
- L2 normalization required for cosine similarity

**Acceptance criteria**:
- `HashEmbedder` implements `Embedder`
- Deterministic: same input always produces same output
- Output is L2 normalized (norm = 1.0)
- Dimension matches configuration (default 384)

---

#### sem.emb.canon
**Type**: task | **Priority**: P1 | **Depends on**: sem.emb.trait

**Purpose**: Implement canonicalization pipeline for consistent embedding input.

**Background**: Raw agent logs contain noise that hurts embedding quality:
- Markdown formatting (`**bold**`, `[links](url)`)
- Huge code blocks with repetitive patterns
- Tool call transcripts
- Progress indicators

Canonicalization produces a clean, consistent text for embedding.

**Algorithm**:
1. Strip markdown formatting (keep text content)
2. Collapse code blocks: keep first 20 + last 10 lines, replace middle with `[code omitted]`
3. Normalize whitespace (collapse runs, trim)
4. Filter low-signal content ("OK", "Done.", empty strings)
5. Truncate to MAX_EMBED_CHARS (default 2000)

**Critical**: Canonicalization must be deterministic! Content hash depends on it.

**Configuration**:
```bash
CASS_SEM_MAX_CHARS=2000
CASS_SEM_CODE_HEAD_LINES=20
CASS_SEM_CODE_TAIL_LINES=10
```

**Acceptance criteria**:
- `canonicalize_for_embedding(raw: &str) -> String`
- `content_hash(raw: &str) -> [u8; 32]` uses canonical text
- Deterministic (same input = same output)
- Handles edge cases (empty, all-code, no-code, unicode)

---

#### sem.vec.fmt
**Type**: task | **Priority**: P1 | **Depends on**: none (parallel with embedder)

**Purpose**: Design and implement the CVVI binary vector index format.

**Background**: We need persistent vector storage that's:
- Fast to load (mmap-friendly)
- Compact (f16 quantization)
- Self-describing (embedder ID in header)
- Corruption-resistant (CRC32, atomic writes)

**Binary format**:
```
Header (variable size):
  Magic: "CVVI" (4 bytes)
  Version: u16
  EmbedderID length: u16
  EmbedderID: string
  Dimension: u32
  Quantization: u8 (0=f32, 1=f16)
  Count: u32
  HeaderCRC32: u32

Rows (Count × ROW_SIZE bytes, fixed):
  MessageID: u64          # Stable SQLite PK
  CreatedAtMs: i64        # For time filtering + recency
  AgentID: u32            # For agent filtering
  WorkspaceID: u32        # For workspace filtering
  SourceID: u32           # For source filtering
  ChunkIdx: u8            # 0 for single-chunk
  VecOffset: u64          # Offset into vector slab
  ContentHash: [u8; 32]   # SHA256(canonical)

Vector slab (Count × Dimension × bytes_per_quant):
  Contiguous f16/f32 values
```

**Why MessageID instead of (source_path, msg_idx)?**
- More stable across file moves
- Works for remote sources where paths differ
- Simpler joins with SQLite

**Acceptance criteria**:
- Header parsing/writing with version compatibility
- CRC32 validation on load
- Documented format in code comments
- Endianness: little-endian throughout

---

### Layer 2: Storage & Operations

#### sem.vec.ops
**Type**: task | **Priority**: P1 | **Depends on**: sem.vec.fmt, sem.emb.hash

**Purpose**: Implement vector index operations (create, load, save, search).

**Core operations**:
1. **Create**: Build index from embeddings + metadata
2. **Load**: mmap from disk, validate header
3. **Save**: Atomic write (temp → fsync → rename)
4. **Search**: Brute-force dot product with filter

**Atomic write pattern**:
```rust
fn save(&self, path: &Path) -> Result<()> {
    let temp = path.with_extension("cvvi.tmp");
    // Write to temp
    let mut f = File::create(&temp)?;
    self.write_to(&mut f)?;
    f.sync_all()?;
    // fsync directory
    File::open(temp.parent().unwrap())?.sync_all()?;
    // Atomic rename
    std::fs::rename(&temp, path)?;
    Ok(())
}
```

**f16 quantization**:
- Use `half` crate for f16 ↔ f32 conversion
- Quantize on write, dequantize on read
- Quality loss is negligible for cosine similarity

**Acceptance criteria**:
- Roundtrip test: save → load preserves all data
- Atomic write: crash during write doesn't corrupt
- mmap loading for large indices
- f16 vs f32 rankings are equivalent (same top-k)

---

#### sem.vec.filt
**Type**: task | **Priority**: P2 | **Depends on**: sem.vec.ops

**Purpose**: Implement inline filter parity for semantic search.

**Background**: Existing cass filters (agent, workspace, source, time) must work identically in semantic mode. Users expect F10 cycling to work.

**Implementation**:
```rust
pub struct SemanticFilter {
    pub agents: Option<HashSet<u32>>,
    pub workspaces: Option<HashSet<u32>>,
    pub sources: Option<HashSet<u32>>,
    pub created_from: Option<i64>,  // ms timestamp
    pub created_to: Option<i64>,
}

impl SemanticFilter {
    pub fn matches(&self, row: &VectorRow) -> bool {
        // Fast integer comparisons, no DB lookup
        if let Some(agents) = &self.agents {
            if !agents.contains(&row.agent_id) { return false; }
        }
        // ... similar for workspace, source, time
        true
    }
}
```

**Conversion**: Need to map existing `SearchFilters` (uses string agent names) to `SemanticFilter` (uses integer IDs). Lookup table built at startup.

**Acceptance criteria**:
- `SemanticFilter::from_search_filters()` conversion
- Filter matches work correctly for all filter types
- No DB queries during filter evaluation
- Performance: <1ms for 50k candidates

---

### Layer 3: ML Embedder & Model Management

#### sem.emb.ml
**Type**: task | **Priority**: P1 | **Depends on**: sem.emb.trait, sem.emb.canon

**Purpose**: Integrate fastembed-rs for real ML embeddings.

**Model**: `sentence-transformers/all-MiniLM-L6-v2`
- 384 dimensions
- ~23MB ONNX model
- ~15ms per embedding on CPU
- Good quality for code/technical content

**Integration**:
```rust
use fastembed::{TextEmbedding, EmbeddingModel, InitOptions};

pub struct FastEmbedder {
    model: TextEmbedding,
    id: String,
}

impl FastEmbedder {
    pub fn new(model_path: &Path) -> Result<Self> {
        let model = TextEmbedding::try_new(InitOptions {
            model_name: EmbeddingModel::AllMiniLML6V2,
            cache_dir: model_path.to_path_buf(),
            show_download_progress: false, // We handle progress
            ..Default::default()
        })?;
        Ok(Self { model, id: "minilm-384".into() })
    }
}
```

**Important**: Model loading should NOT auto-download! We control downloads via model_manager.

**Acceptance criteria**:
- `FastEmbedder` implements `Embedder`
- Loads from local cache only (no auto-download)
- Returns error if model not present
- `is_semantic()` returns true

---

#### sem.mod.core
**Type**: task | **Priority**: P2 | **Depends on**: sem.emb.ml

**Purpose**: Implement complete model management (manifest, state machine, download, verify).

**This is a larger bead combining**: manifest, state machine, download, verification.

**Model manifest** (`models.manifest.toml` in repo):
```toml
[[models]]
id = "all-minilm-l6-v2"
repo = "sentence-transformers/all-MiniLM-L6-v2"
revision = "e4ce9877abf3edfe10b0d82785e83bdcb973e22e"  # Pinned!
files = [
    { name = "model.onnx", sha256 = "...", size = 22713856 },
    { name = "tokenizer.json", sha256 = "...", size = 711396 },
    { name = "config.json", sha256 = "...", size = 612 },
]
license = "Apache-2.0"
```

**State machine**:
```rust
pub enum ModelState {
    NotInstalled,
    NeedsConsent,
    Downloading { progress_pct: u8, bytes: u64, total: u64 },
    Verifying,
    Ready,
    Disabled { reason: String },
    VerificationFailed { reason: String },
}
```

**Download system**:
- Resumable (HTTP Range header)
- Progress reporting via channel
- Exponential backoff on failure (3 retries)
- Timeout: 5 minutes per file

**Verification + atomic install**:
- Download to `models/<name>.downloading/`
- Verify SHA256 for each file
- Atomic rename to `models/<name>/`
- Write `.verified` marker

**Acceptance criteria**:
- Full download → verify → install flow works
- Partial download resumes correctly
- Corrupt download detected and retried
- State transitions are correct
- No network calls without explicit consent

---

### Layer 4: Search Integration

#### hyb.search
**Type**: task | **Priority**: P1 | **Depends on**: sem.vec.ops, sem.vec.filt

**Purpose**: Implement semantic search execution and SearchMode enum.

**SearchMode enum**:
```rust
#[derive(Clone, Copy, Debug, Default)]
pub enum SearchMode {
    #[default]
    Lexical,
    Semantic,
    Hybrid,
}

impl SearchMode {
    pub fn next(self) -> Self {
        match self {
            Lexical => Semantic,
            Semantic => Hybrid,
            Hybrid => Lexical,
        }
    }
}
```

**Semantic search flow**:
1. Canonicalize query text
2. Embed query (ML or hash)
3. Build SemanticFilter from current SearchFilters
4. Search vector index with filter
5. Map MessageID results back to full hits via SQLite

**Query cache**:
```rust
pub struct QueryCache {
    embeddings: LruCache<String, Vec<f32>>,  // query → embedding
}
```
- Cache key: canonical query text
- Cache size: 100 queries (configurable)
- Invalidate on embedder change

**Acceptance criteria**:
- `search_semantic()` returns ranked results
- Filters are honored (agent/workspace/source/time)
- Query cache reduces latency on repeated queries
- Graceful error if semantic unavailable

---

#### hyb.rrf
**Type**: task | **Priority**: P1 | **Depends on**: hyb.search

**Purpose**: Implement Reciprocal Rank Fusion for hybrid search.

**RRF formula**: `score(d) = Σ 1/(k + rank(d))` where k=60

**Implementation**:
```rust
const RRF_K: f32 = 60.0;

pub fn rrf_fuse(
    lexical: &[SearchHit],
    semantic: &[VectorSearchResult],
    limit: usize,
) -> Vec<HybridSearchHit> {
    let mut scores: HashMap<u64, HybridScore> = HashMap::new();  // MessageID → score

    for (rank, hit) in lexical.iter().enumerate() {
        let entry = scores.entry(hit.message_id).or_default();
        entry.rrf += 1.0 / (RRF_K + rank as f32 + 1.0);
        entry.lexical_rank = Some(rank);
    }

    for (rank, hit) in semantic.iter().enumerate() {
        let entry = scores.entry(hit.message_id).or_default();
        entry.rrf += 1.0 / (RRF_K + rank as f32 + 1.0);
        entry.semantic_rank = Some(rank);
    }

    // Sort by RRF score descending
    let mut results: Vec<_> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.rrf.partial_cmp(&a.1.rrf).unwrap());
    results.truncate(limit);
    // ... convert to HybridSearchHit
}
```

**Candidate depth**: Fetch 3× limit from each source for better fusion.

**Acceptance criteria**:
- Documents appearing in both lists get higher scores
- Rankings are stable (deterministic)
- Handles disjoint result sets gracefully
- Performance: <5ms for 500 candidates

---

#### hyb.rank
**Type**: task | **Priority**: P2 | **Depends on**: hyb.rrf

**Purpose**: Apply RankingMode (Recent/Balanced/Relevance) in semantic/hybrid modes.

**Background**: Users expect F12 (RankingMode) to work across all search modes.

**Semantic mode ranking**:
- Map similarity [-1, 1] to [0, 1]: `sim01 = (sim + 1) / 2`
- Apply RankingMode weights:
  - Recent Heavy: `0.3 * sim01 + 0.7 * recency`
  - Balanced: `0.5 * sim01 + 0.5 * recency`
  - Relevance Heavy: `0.8 * sim01 + 0.2 * recency`
  - Match Quality: `0.85 * sim01 + 0.15 * recency`
  - Date Newest/Oldest: Sort by date, ignore sim

**Hybrid mode ranking**:
- Primary: RRF score
- Tie-break: RankingMode preference
- Tie-break 2: Higher max(lexical_bm25, semantic_sim)

**Acceptance criteria**:
- All RankingMode values work in Semantic mode
- All RankingMode values work in Hybrid mode
- Rankings match user expectations
- No regression in Lexical mode

---

#### hyb.filt
**Type**: task | **Priority**: P2 | **Depends on**: hyb.search

**Purpose**: Ensure filter parity between Lexical and Semantic/Hybrid.

**This is validation + edge case handling, not new functionality.**

**Validation checklist**:
- [ ] F10 (agent filter) works in Semantic
- [ ] F10 works in Hybrid
- [ ] Workspace filter (--workspace) works
- [ ] Source filter (--source) works
- [ ] Time filter (F6/F7) works
- [ ] Combined filters work
- [ ] "All" filter resets correctly

**Edge cases**:
- Agent with no indexed messages → empty results (not error)
- Time range outside indexed range → empty results
- Filter changes mid-session → re-search works

**Acceptance criteria**:
- All filter combinations tested
- No crashes or panics on edge cases
- Results are correct (verified against lexical)

---

### Layer 5: User Interface

#### tui.sem.mode
**Type**: task | **Priority**: P1 | **Depends on**: hyb.search

**Purpose**: Implement Alt+S keyboard shortcut for mode cycling.

**Key binding**: `Alt+S` (mnemonic: Search mode)

**Behavior**:
- Press Alt+S → cycle mode (LEX → SEM → HYB → LEX)
- If switching to SEM/HYB and model not installed:
  - Show install prompt (see tui.sem.prompt)
  - Don't change mode until consent given
- If model is downloading:
  - Show toast "Model downloading..."
  - Stay on current mode

**Status bar indicator**:
- `LEX` - default color
- `SEM` - cyan (ML active)
- `SEM*` - cyan with asterisk (hash fallback)
- `HYB` - magenta

**State persistence**:
- Save search_mode to config
- Restore on startup

**Acceptance criteria**:
- Alt+S cycles modes
- Status bar updates correctly
- Mode persists across sessions
- Help screen (F1) documents Alt+S

---

#### tui.sem.state
**Type**: task | **Priority**: P1 | **Depends on**: tui.sem.mode, sem.mod.core

**Purpose**: Track SemanticAvailability state in TUI.

**State enum**:
```rust
pub enum SemanticAvailability {
    NotInstalled,          // Model not on disk
    NeedsConsent,          // Prompt should appear
    Downloading { pct: u8 }, // In progress
    Ready,                 // ML ready to use
    HashFallback,          // User opted for hash
    Disabled { reason: String }, // Offline/policy
}
```

**State transitions**:
- App starts → check model → NotInstalled or Ready
- User presses Alt+S to SEM → NeedsConsent (if NotInstalled)
- User presses D → Downloading
- Download completes → Ready
- User presses H → HashFallback

**Integration with model_manager**:
- Subscribe to ModelState changes
- Update SemanticAvailability accordingly
- Handle async state updates

**Acceptance criteria**:
- State is always accurate
- UI reflects current state
- No race conditions on state changes

---

#### tui.sem.prompt
**Type**: task | **Priority**: P1 | **Depends on**: tui.sem.state

**Purpose**: Implement consent dialog for model download.

**Dialog appearance** (modal popup):
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
- `D` → Start download, close prompt, show progress in status bar
- `H` → Enable hash mode, close prompt, switch to SEM*
- `Esc` → Cancel, close prompt, stay on current mode

**UX considerations**:
- Prompt only appears when user actively switches to SEM/HYB
- Never auto-appears on startup
- Remember choice (don't re-prompt if user chose H)

**Acceptance criteria**:
- Dialog renders correctly
- All keybindings work
- Dialog is dismissable
- Download starts correctly on D

---

#### tui.sem.display
**Type**: task | **Priority**: P2 | **Depends on**: tui.sem.state

**Purpose**: Implement status bar indicators and toast notifications.

**Status bar elements**:
- Mode indicator: `mode:LEX` / `mode:SEM` / `mode:SEM*` / `mode:HYB`
- Download progress (when active): `⬇️ 45%`
- Embedder info (optional): `emb:minilm`

**Toast notifications**:
- "Semantic search ready" - when ML model becomes available
- "Semantic index rebuilt" - after index upgrade
- "Download failed: {reason}" - on error with retry info
- "Using hash fallback" - when switching to hash mode

**Toast behavior**:
- Auto-dismiss after 3 seconds
- Don't stack more than 2 toasts
- Newer toast replaces older

**Acceptance criteria**:
- Status bar shows correct mode
- Download progress visible
- Toasts appear and dismiss correctly
- No UI glitches during state changes

---

### Layer 6: CLI Support

#### cli.models
**Type**: task | **Priority**: P2 | **Depends on**: sem.mod.core

**Purpose**: Implement `cass models` subcommand for model management.

**Commands**:
```bash
# Show model status
cass models status [--json]
# Output: state, model_id, size, download progress

# Install/download model
cass models install [--model all-minilm-l6-v2] [--mirror URL]

# Verify model integrity
cass models verify [--repair]

# Remove model files
cass models remove [--model all-minilm-l6-v2] [-y]
```

**Use cases**:
- Pre-provision model before first TUI use
- Verify model in CI/automated environments
- Cleanup disk space

**JSON output** (for scripting):
```json
{
  "state": "ready",
  "model_id": "all-minilm-l6-v2",
  "model_path": "/Users/x/.local/share/coding-agent-search/models/all-MiniLM-L6-v2",
  "size_bytes": 23000000,
  "verified": true
}
```

**Acceptance criteria**:
- All commands work correctly
- JSON output is parseable
- Install works in headless environments
- Verify catches corruption

---

#### cli.search.sem
**Type**: task | **Priority**: P2 | **Depends on**: hyb.search, hyb.rrf

**Purpose**: Add --mode flag to search command and update robot output.

**New flag**:
```bash
cass search "query" --mode lexical|semantic|hybrid
```

**Robot output schema** (--robot mode):
```json
{
  "hits": [{
    "message_id": 12345,
    "source_path": "...",
    "agent": "claude-code",
    "scores": {
      "lexical_rank": 3,
      "semantic_rank": 1,
      "rrf_score": 0.0328,
      "lexical_bm25": 12.5,
      "semantic_similarity": 0.89
    }
  }],
  "_meta": {
    "search_mode": "hybrid",
    "embedder": "minilm-384",
    "embedder_is_semantic": true,
    "lexical_candidates": 150,
    "semantic_candidates": 150,
    "filters_applied": {...}
  }
}
```

**Acceptance criteria**:
- --mode flag works correctly
- Robot output includes all score components
- Error handling for semantic unavailable
- Help text documents new flag

---

### Layer 7: Testing

#### tst.sem.unit
**Type**: task | **Priority**: P2 | **Depends on**: all implementation beads

**Purpose**: Comprehensive unit test coverage.

**Test categories**:

**Embedder tests**:
- `test_hash_embedder_deterministic`
- `test_hash_embedder_dimension`
- `test_hash_embedder_normalized`
- `test_fastembed_loads_model`
- `test_embedder_trait_consistency`

**Canonicalization tests**:
- `test_canonicalize_strips_markdown`
- `test_canonicalize_collapses_code`
- `test_canonicalize_deterministic`
- `test_content_hash_stability`

**Vector index tests**:
- `test_vector_index_roundtrip`
- `test_vector_index_atomic_write`
- `test_vector_index_crc_validation`
- `test_vector_index_f16_quantization`
- `test_vector_index_filter_parity`

**RRF tests**:
- `test_rrf_fusion_ordering`
- `test_rrf_handles_disjoint_sets`
- `test_rrf_tie_breaking`
- `test_rrf_candidate_depth`

**Model management tests**:
- `test_model_state_transitions`
- `test_model_verification_catches_corruption`
- `test_model_atomic_install`
- `test_consent_gated_download`

**Acceptance criteria**:
- All tests pass
- Coverage > 80% for new code
- Tests are fast (< 10s total for unit tests)

---

#### tst.sem.int
**Type**: task | **Priority**: P2 | **Depends on**: tst.sem.unit

**Purpose**: Integration tests for end-to-end flows.

**Test scenarios**:
- `test_semantic_search_returns_results`
- `test_hybrid_search_improves_recall`
- `test_incremental_index_skips_unchanged`
- `test_search_mode_persists`
- `test_filter_parity_semantic_vs_lexical`
- `test_tui_install_prompt_shown`
- `test_offline_mode_disables_download`
- `test_robot_output_schema`

**Acceptance criteria**:
- All integration tests pass
- Tests use real (small) test fixtures
- Tests don't require network (mock download)

---

#### tst.sem.bench
**Type**: task | **Priority**: P3 | **Depends on**: tst.sem.int

**Purpose**: Performance benchmarks for regression detection.

**Benchmarks**:
- `bench_hash_embed_1000_docs`
- `bench_fastembed_embed_100_docs`
- `bench_vector_search_10k`
- `bench_vector_search_50k_filtered`
- `bench_rrf_fusion_100_results`
- `bench_canonicalize_long_message`

**Target latencies**:
- Hash embed: <1ms per doc
- ML embed: <20ms per doc
- Vector search 10k: <5ms
- Vector search 50k: <20ms
- RRF fusion: <5ms

**Acceptance criteria**:
- Benchmarks run via `cargo bench`
- Results logged for comparison
- No > 20% regression from baseline

---

## Implementation Order

**Critical path** (must be done sequentially):
1. sem.emb.trait (Day 1)
2. sem.emb.hash (Day 1-2)
3. sem.emb.canon (Day 2)
4. sem.vec.fmt (Day 2-3)
5. sem.vec.ops (Day 3-4)
6. hyb.search (Day 4-5)
7. hyb.rrf (Day 5)
8. tui.sem.mode (Day 5-6)

**Can be parallelized**:
- sem.emb.ml || sem.vec.filt (after sem.vec.ops)
- sem.mod.core || hyb.rank (after hyb.rrf)
- tui.sem.* || cli.* (after hyb.search)
- tst.* (after implementation complete)

**Estimated total**: 8-10 days with one developer, 4-5 days with two parallelizing.

---

## Success Metrics

1. **Search quality**: Semantic finds relevant results that lexical misses
2. **Performance**: <100ms query latency for 50k corpus
3. **User satisfaction**: Seamless mode switching, clear indicators
4. **Reliability**: No crashes, data corruption, or stuck states
5. **Privacy**: No network calls without explicit consent

---

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| fastembed API changes | Low | High | Pin version, test on upgrade |
| Model download fails | Medium | Low | Hash fallback, retry logic |
| Index corruption | Low | High | CRC32, atomic writes, backup |
| OOM on large corpus | Low | Medium | mmap, streaming, configurable batch size |
| User confusion on modes | Medium | Low | Clear status indicators, help text |

---

## Future Enhancements (Not in Initial Scope)

1. **HNSW index** - For corpora >100k, add approximate nearest neighbor
2. **Multi-chunk messages** - Better recall for long documents
3. **Diversity penalty** - Reduce same-source clustering
4. **Weight presets** - User-tunable hybrid fusion
5. **API embedders** - OpenAI, Cohere options for cloud users
6. **"More like this"** - Find similar messages by embedding
