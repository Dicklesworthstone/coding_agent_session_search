# 🔎 coding-agent-search (cass)

<div align="center">
  <img src="docs/assets/images/cass_illustration.webp" alt="coding-agent-search (cass) illustration">
</div>

![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-blue.svg)
![Rust](https://img.shields.io/badge/Rust-pinned%20nightly-orange.svg)
![Status](https://img.shields.io/badge/status-alpha-purple.svg)
[![Coverage](https://codecov.io/gh/Dicklesworthstone/coding_agent_session_search/branch/main/graph/badge.svg)](https://codecov.io/gh/Dicklesworthstone/coding_agent_session_search)
![License](https://img.shields.io/badge/license-MIT%2BOpenAI%2FAnthropic%20Rider-green.svg)

**Unified, high-performance TUI to index and search your local coding agent history.**
Aggregates sessions from Codex, Claude Code, Gemini CLI, Cline, OpenCode, Amp, Cursor, ChatGPT, Aider, Pi-Agent, Oh My Pi, GitHub Copilot Chat, Copilot CLI, OpenClaw, Clawdbot, Vibe, Crush, Goose, Hermes, Kimi Code, Muse Code, Qwen Code, Factory (Droid), Antigravity, OpenHands, and Grok Build into a single, searchable timeline.

<div align="center">

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/coding_agent_session_search/main/install.sh?$(date +%s)" \
  | bash -s -- --easy-mode --verify
```

```powershell
# Windows (PowerShell)
& ([scriptblock]::Create((irm "https://raw.githubusercontent.com/Dicklesworthstone/coding_agent_session_search/main/install.ps1"))) -EasyMode -Verify
```

Installs the latest release by default. Pass `--version <tag>` / `-Version <tag>` to pin a specific version.

**Or via package managers:**

```bash
# Homebrew (Apple Silicon macOS + Linux)
brew install dicklesworthstone/tap/cass

# Windows (Scoop)
scoop bucket add dicklesworthstone https://github.com/Dicklesworthstone/scoop-bucket
scoop install dicklesworthstone/cass
```

The Homebrew tap installs prebuilt release tarballs (not bottles) for Linux and Apple Silicon macOS. On Intel macOS, use the install script with `--from-source`.

</div>

---

## 🤖 Agent Quickstart (Robot Mode)

⚠️ **Never run bare `cass` in an agent context** — it launches the interactive TUI. Always use `--robot` or `--json`.

```bash
# 1) One-shot agent triage. Follow next_command when present.
cass triage --json
#    From zero context, `cass --json` and `cass --robot` also resolve to triage.

# Verify a newly installed executable without opening the configured archive.
cass selftest --json
# `health --binary-only` still reports (and therefore probes) archive readiness.

# 2) Search across all agent history. Default search is hybrid-preferred:
#    lexical is the fast required path; semantic refinement joins when ready.
cass search "authentication error" --robot --limit 5 --fields minimal

# 3) Find the current or recent session for this workspace
cass sessions --current --json
cass sessions --workspace "$(pwd)" --json --limit 5

# 4) View + expand a hit (use source_path/line_number from search output)
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json

# 5) Discover the full machine API
cass capabilities --json
cass robot-docs guide
cass robot-docs schemas

# 6) Exclude a noisy agent harness from future indexing
cass sources agents list --json
cass sources agents exclude openclaw
cass sources agents include openclaw
```

**Output conventions**
- stdout = data only
- stderr = diagnostics
- exit 0 = success

**Search asset contract**
- SQLite is the source of truth for indexed conversations and messages. All derived assets (lexical index, semantic vectors, analytics rollups, retention backups) can be rebuilt from SQLite; no derived asset is authoritative.
- Lexical search is the required fast path. Missing, stale, or incompatible lexical assets are treated as derived-state problems that cass should rebuild from SQLite instead of asking operators to perform routine manual repair.
- Hybrid is the default search intent. Robot metadata (`--robot --robot-meta`) reports the requested mode, realized mode, semantic refinement status, and any lexical fallback reason when semantic assets are not ready.
- Semantic assets are opportunistic background enrichment. Lexical-only results are expected during first indexing, semantic catch-up, disabled semantic policy, or unavailable local model/vector files.
- Semantic model acquisition is **opt-in**: `cass models install` downloads the default `all-minilm-l6-v2` (alias `minilm`, ~90 MB) only on explicit request; `--model multilingual-minilm` selects the larger multilingual MiniLM L12 model (~480 MB) for CJK/mixed-language archives. Cass never auto-downloads or auto-selects the multilingual space. Air-gapped installs use `--from-file <dir>`. While the selected model is absent, hybrid search uses lexical-only and reports `fallback_mode="lexical"` in health/status.
- `cass triage --json` is the safest first command for agents: it combines readiness, `next_command`, `recommended_commands[]`, docs/schema pointers, starter workflows, and accepted recoveries. `cass health --json` and `cass status --json` remain the narrower truth surfaces for readiness, active rebuilds, and recovery.

**Lexical publish durability (atomic-swap)**
- Every lexical publish is an atomic renameat2(RENAME_EXCHANGE) on Linux, or a parked-rename + restore-on-failure dance elsewhere. Readers never see a half-torn index — they see either the old or the new generation, never a mix. See `src/indexer/mod.rs::publish_staged_lexical_index`.
- The prior-live generation is retained under `<data_dir>/index/.lexical-publish-backups/<dated>/` for a bounded retention window. Default cap is `1` (keep just the most-recent prior generation for one-step rollback); override via the `CASS_LEXICAL_PUBLISH_BACKUP_RETENTION` env var (`0` disables retention entirely, higher N keeps deeper history). Pruning runs after every successful publish and emits structured `tracing::info!` events with `freed_bytes` + `retention_limit` for observability.
- Crash recovery is automatic: a crash between the atomic swap and the retain-rename is handled by `recover_or_finalize_interrupted_lexical_publish_backup` on the next startup, which moves any orphaned canonical sidecar (`.<name>.publish-in-progress.bak`) into `.lexical-publish-backups/` before the next publish lands.

**Quarantine, GC, and the doctor/diag surface**
- Corrupt or failed-validation assets are quarantined rather than auto-deleted. `cass diag --json --quarantine` enumerates every quarantined artifact (failed seed bundles, retained publish backups, quarantined lexical generations) with `size_bytes`, `age_seconds`, `safe_to_gc`, and a human-readable `gc_reason`. The `safe_to_gc` flag is **advisory** — it reflects retention policy + cleanup dry-run eligibility and is not wired to any automatic deletion path.
- `cass doctor --json` surfaces the same quarantine summary plus `checks[]` status for every diagnostic the tool runs. Without `--fix`, doctor is read-only (`auto_fix_applied=false`, `auto_fix_actions=[]`, `issues_fixed=0`); with `--fix` it applies only the repairs whose dry-run plans are proven safe (currently: Track A analytics rebuild, Track B rollup rebuild via `rebuild_token_daily_stats` when the `token_usage` ledger is intact).
- Lexical generation cleanup uses a dispositions + inspection-required-first policy. Operators running `cass doctor --fix` never have a generation reclaimed silently — every quarantine stays on disk until an explicit derived-asset rebuild (`cass models backfill` or an index refresh recommended by `cass health --json`) supersedes it.
- A derived (SQLite fallback) FTS repair that fails **identically on 5 consecutive `cass index` runs** escalates from a warning to a non-zero exit ([#434](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/434)): the counter persists in `<data_dir>/index/.fts-repair-failure-streak.json`, watch daemons log the escalation instead of exiting, and any run whose repair succeeds — or fails differently — resets it. Canonical rows and the Tantivy index are unaffected; run `cass doctor --rebuild-canonical-fts --yes --json` for the explicit repair.

**Schema stability guarantees**
- The JSON contract surfaces (`triage`, `capabilities`, `selftest`, `health`, `status`, `diag`, `models status`, `models verify`, `models check-update`, `introspect`, `doctor`, `api-version`, `stats`, `sessions`, `search`, `pack`, `swarm status`, `swarm work-packet`, `swarm lint`) are pinned by golden-file regression tests under `tests/golden/robot/`. A change to any field name, type, or nullability fails the golden test suite and requires a deliberate regeneration pass (`UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-golden-target cargo test --test golden_robot_json --test golden_robot_docs`).
- `cass introspect --json`'s `response_schemas` block enumerates every schema in a stable alphabetical order (`BTreeMap`-backed — see bead coding_agent_session_search-8sl73).
- Error envelopes (`{error: {code, kind, message, hint, retryable}}`) have a fixed shape. `kind` values are kebab-case; branch on `err.kind`, not on the numeric code, for codes ≥ 10 (see the Error Handling section below).

## 📬 Agent Mail Fallback (When MCP Tools Are Not Exposed)

If your runtime does not expose built-in `mcp-agent-mail` tools (for example, `list_mcp_resources` is empty), you can still coordinate via direct MCP HTTP calls.

### 1) Start the local Agent Mail server

```bash
~/.local/pipx/venvs/mcp-agent-mail/bin/python -m mcp_agent_mail.cli serve-http --host 127.0.0.1 --port 8765
```

### 2) Use the Streamable HTTP MCP endpoint (`/mcp`)

```bash
curl -sS -X POST http://127.0.0.1:8765/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"health","method":"tools/call","params":{"name":"health_check","arguments":{}}}'
```

### 3) Minimal coordination flow (project -> agent -> message -> inbox -> ack)

```bash
# Ensure project
curl -sS -X POST http://127.0.0.1:8765/mcp -H 'Content-Type: application/json' -d \
'{"jsonrpc":"2.0","id":"ensure","method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/data/projects/coding_agent_session_search"}}}'

# Register agent
curl -sS -X POST http://127.0.0.1:8765/mcp -H 'Content-Type: application/json' -d \
'{"jsonrpc":"2.0","id":"register","method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/data/projects/coding_agent_session_search","program":"codex","model":"gpt-5","name":"YourAgentName"}}}'

# Send message
curl -sS -X POST http://127.0.0.1:8765/mcp -H 'Content-Type: application/json' -d \
'{"jsonrpc":"2.0","id":"send","method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/data/projects/coding_agent_session_search","sender_name":"YourAgentName","to":["PeerAgent"],"subject":"[coord] hello","thread_id":"coord-2026-02-13","ack_required":true,"body_md":"Online and starting work."}}}'

# Fetch inbox
curl -sS -X POST http://127.0.0.1:8765/mcp -H 'Content-Type: application/json' -d \
'{"jsonrpc":"2.0","id":"inbox","method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/data/projects/coding_agent_session_search","agent_name":"YourAgentName","limit":50,"include_bodies":true}}}'

# Acknowledge message id 42
curl -sS -X POST http://127.0.0.1:8765/mcp -H 'Content-Type: application/json' -d \
'{"jsonrpc":"2.0","id":"ack","method":"tools/call","params":{"name":"call_extended_tool","arguments":{"tool_name":"acknowledge_message","arguments":{"project_key":"/data/projects/coding_agent_session_search","agent_name":"YourAgentName","message_id":42}}}}'
```

### Important caveat

`mcp_agent_mail` defaults to `sqlite+aiosqlite:///./storage.sqlite3`. That means the server working directory determines which mailbox database you are using. To avoid "project not found" confusion, start the server from the same directory your team expects for mailbox state.

## 📸 Screenshots

<div align="center">

### Search Results Across All Your Agents
*Three-pane layout with semantic styling: filter bar with pills, results list with color-coded agents and score tiers, and syntax-highlighted detail preview with tab navigation*

<img src="docs/assets/screenshots/screenshot_01.webp" alt="Main TUI showing search results across multiple coding agents" width="800">

---

### Rich Conversation Detail View
*Full conversation rendering with markdown formatting, code blocks, headers, and structured content*

<img src="docs/assets/screenshots/screenshot_02.webp" alt="Detail view showing formatted conversation content" width="800">

---

### Quick Start & Keyboard Reference
*Built-in help screen (press `F1` or `?`) with all shortcuts, filters, modes, and navigation tips*

<img src="docs/assets/screenshots/screenshot_03.webp" alt="Help screen showing keyboard shortcuts and features" width="500">

</div>

---

## 💡 Why This Exists

### The Problem

AI coding agents are transforming how we write software. Claude Code, Codex, Cursor, Copilot, Aider, Pi-Agent; each creates a trail of conversations, debugging sessions, and problem-solving attempts. But this wealth of knowledge is **scattered and unsearchable**:

- **Fragmented storage**: Each agent stores data differently—JSONL files, SQLite databases, markdown logs, proprietary JSON formats
- **No cross-agent visibility**: Solutions discovered in Cursor are invisible when you're using Claude Code
- **Lost context**: That brilliant debugging session from two weeks ago? Good luck finding it by scrolling through files
- **No semantic search by default**: File-based grep doesn't understand natural language queries; cass can add optional local ML search when model files are installed

### The Solution

`cass` treats your coding agent history as a **unified knowledge base**. It:

1. **Normalizes** disparate formats into a common schema
2. **Indexes** everything with a purpose-built full-text search engine
3. **Surfaces** relevant past conversations in milliseconds
4. **Respects** your privacy—everything stays local, nothing phones home

### Who Benefits

- **Individual developers**: Find that solution you know you've seen before
- **Teams**: Share institutional knowledge across different tool preferences
- **AI agents themselves**: Let your current agent learn from all your past agents (via robot mode)
- **Power users**: Build workflows that leverage your complete coding history

---

## ✨ Key Features

### ⚡ Instant Search (Sub-60ms Latency)
- **"Search-as-you-type"**: Results update instantly with every keystroke.
- **Edge N-Gram Indexing**: We frontload the work by pre-computing prefix matches (e.g., "cal" -> "calculate") during indexing, trading disk space for O(1) lookup speed at query time.
- **Smart Tokenization**: Handles `snake_case` ("my_var" matches "my" and "var"), hyphenated terms, and code symbols (`c++`, `foo.bar`) correctly.
- **Zero-Stall Updates**: The background indexer commits changes atomically; `reader.reload()` ensures new messages appear in the search bar immediately without restarting.
- **One-shot CLI overhead**: the sub-60ms figure is the engine query. A one-shot `cass search --robot` currently spends roughly a second in archive open and integrity preflight on a ~10 GB archive; `--robot-meta` reports that separately as `_meta.timing.other_ms`, while `search_ms` stays in the tens of milliseconds.

### 🧠 Optional Semantic Search (Local Inference, No Network at Query Time)
- **Local inference**: Uses frankensearch's pure-Rust native MiniLM implementation with local safetensors weights. Once MiniLM is installed, no network traffic is required to answer queries.
- **Warm-daemon reuse**: Semantic and hybrid CLI searches automatically use an
  already-running local embedding daemon (including a socket selected with
  `CASS_DAEMON_SOCKET`) and only initialize the installed in-process model if
  daemon inference fails. Pass `--daemon` to permit auto-spawning a missing
  daemon, or `--no-daemon` to force direct inference. `--fast-only` stays in
  the deterministic hash-vector space. Each data directory gets a distinct
  default socket and owner-private pinned key; fresh handshake, health,
  embedding, batch, and rerank challenges authenticate the exact response and
  immutable Frankensearch embedding identity before any daemon output is used.
  `--two-tier` progressive refinement (fast results refined in place by the
  quality tier) is experimental and currently inactive: the one-shot CLI
  collapses it to a single-tier quality search and the TUI's progressive lanes
  are disabled at HEAD, so hybrid search today is lexical plus one MiniLM
  refinement pass when the model is installed.
- **Opt-in acquisition**: `cass models install` downloads `all-minilm-l6-v2` from Hugging Face on explicit request and verifies SHA256 checksums. `cass models install --model multilingual-minilm` explicitly selects `paraphrase-multilingual-MiniLM-L12-v2` for CJK and mixed-language retrieval. Nothing is fetched until an install command runs, and merely installing the multilingual model never changes the active space.

- **Air-gapped install**: `cass models install --model <minilm|multilingual-minilm> --from-file <dir>` accepts a pre-downloaded model directory so you can bring the assets in yourself.
- **Switching spaces**: both models output 384 values, but their identities and vectors are incompatible. Set `CASS_SEMANTIC_EMBEDDER=multilingual-minilm`, then run `cass models backfill --tier quality --embedder multilingual-minilm`; cass keeps lexical fail-open active until the complete new generation is atomically published.
- **Required files** (all must be present after install; `cass models verify --model <minilm|multilingual-minilm>` checks the selected model):
  - `model.safetensors`
  - `tokenizer.json`
  - `config.json`
  - `special_tokens_map.json`
  - `tokenizer_config.json`
- **Vector index**: Stored as `vector_index/index-<embedder>.fsvi` in the data directory.
- **Lexical fail-open**: While the model is absent, `cass` returns lexical-only results and reports `fallback_mode="lexical"` in health/status; search never blocks on semantic assets.

#### Explicit Hash Vector Tier

The deterministic hash embedder is available only when explicitly selected, such as with `--fast-only`, `--embedder hash`, or `CASS_SEMANTIC_EMBEDDER=hash`. It is a separate lexical-feature vector space, not a silent substitute for missing MiniLM vectors:

| Feature | ML Model (MiniLM) | Hash Embedder (FNV-1a) |
|---------|-------------------|------------------------|
| **Meaning Understanding** | ✅ "car" ≈ "automobile" | ❌ Exact tokens only |
| **Initialization Time** | ~500ms (model loading) | <1ms (instant) |
| **Network Dependency** | None (after install) | None |
| **Disk Footprint** | ~90MB model files | 0 bytes |
| **Deterministic** | ✅ Same input = same output | ✅ Same input = same output |

**Algorithm**:
1. **Tokenize**: Lowercase, split on non-alphanumeric, filter tokens <2 characters
2. **Hash**: Apply FNV-1a to each token
3. **Project**: Use hash to determine dimension index and sign (+1 or -1) in a 384-dimensional vector
4. **Normalize**: L2 normalize to unit length for cosine similarity

**When to Use**:
- Quick setup without downloading model files
- Environments where ML inference overhead is unwanted
- Fast-tier testing or an explicitly chosen degraded mode

**Override**: Set `CASS_SEMANTIC_EMBEDDER=hash` to force hash mode even when ML model is available.

#### FSVI Vector Index Format

`cass` uses the **frankensearch FSVI** vector index format (`.fsvi`) for storing semantic embeddings.

**Features**:
- **Memory-mappable**: large indexes open without copying into RAM
- **Quantization**: supports `f32` and `f16` storage for smaller on-disk size
- **Fast search**: exact brute-force vector search by default; HNSW approximate search runs only when `--approximate` is passed and the HNSW sidecar file exists. `hnsw_ready` in `status --json` means only that the sidecar file is present, not that ANN is in use

**Index Location**: `~/.local/share/coding-agent-search/vector_index/index-<embedder>.fsvi`

#### Search Modes

`cass` supports three search modes, selectable via `--mode` flag or `Alt+S` in the TUI:

| Mode | Algorithm | Best For |
|------|-----------|----------|
| **Lexical** | BM25 full-text | Exact term matching, code searches |
| **Semantic** | Vector similarity | Conceptual queries, "find similar" |
| **Hybrid** (default) | Lexical + single-tier semantic refinement fused with RRF; lexical fail-open | Balanced precision and recall |

**Lexical Search**: Uses Quill's BM25 implementation with prefix matching. Best when you know the exact terms you're looking for. The lexical index is derived from SQLite; if it is missing, stale, or incompatible, cass reports the state and rebuilds through the normal indexing path from the canonical database.

**Semantic Search**: Computes vector similarity between query and indexed MiniLM embeddings. Finds conceptually related content even without exact term overlap. Explicit semantic mode requires the MiniLM model and a compatible MiniLM vector index; it never substitutes same-dimensional hash vectors.

**Hybrid Search**: The default. It combines lexical and semantic results using Reciprocal Rank Fusion (RRF) when semantic assets are ready, and it fails open to lexical when semantic enrichment is still catching up or disabled:
```
RRF_score = Σ 1 / (K + rank_i)
```
Where K=60 (tuning constant) and rank_i is the position in each result list. This balances the precision of lexical search with the recall of semantic search. Semantic refinement is a single pass over the installed MiniLM index; progressive two-tier refinement (`--two-tier`) is experimental and currently inactive.

```bash
# CLI examples
cass search "authentication" --mode lexical --robot
cass search "how to handle user login" --mode semantic --robot
cass search "auth error handling" --mode hybrid --robot
```

### 🎯 Advanced Search Features
- **Wildcard Patterns**: Full glob-style pattern support:
  - `foo*` - Prefix match (finds "foobar", "foo123")
  - `*foo` - Suffix match (finds "barfoo", "configfoo")
  - `*foo*` - Substring match (finds "afoob", "configuration")
- **Auto-Fuzzy Fallback**: When exact searches return sparse results, automatically retries with `*term*` wildcards to broaden matches. Visual indicator shows when fallback is active.
- **Query History Deduplication**: Recent searches deduplicated to show unique queries; navigate with `Up`/`Down` arrows.
- **Match Quality Ranking**: New ranking mode (cycle with `F12`) that prioritizes exact matches over wildcard/fuzzy results.
- **Match Highlighting**: Use `--highlight` in robot mode to wrap matching terms with markers (`**bold**` for text, `<mark>` for HTML output).

### 🖥️ Rich Terminal UI (TUI)

Powered by [FrankenTUI (ftui)](https://github.com/Dicklesworthstone/frankentui) — a high-performance Elm-architecture TUI framework with adaptive frame budgets, Bayesian diff selection, and spring-based animations.

- **Three-Pane Layout**: Filter bar (top), scrollable results (left), and syntax-highlighted details (right).
- **Multi-Line Result Display**: Each result shows location and up to 3 lines of context; alternating stripes improve scanability.
- **Live Status**: Footer shows real-time indexing progress—agent discovery count during scanning, then item progress with sparkline visualization (e.g., `📦 Indexing 150/2000 (7%) ▁▂▄▆█`)—plus active filters.
- **Multi-Open Queue**: Queue multiple results with `Ctrl+Enter`, then open all in your editor with `Ctrl+O`. Confirmation prompt for large batches (≥12 items).
- **Find-in-Detail**: Press `/` to search within the detail pane; matches highlighted with `n`/`N` navigation.
- **Mouse Support**: Click to select results, scroll panes, or clear filters.
- **Theming**: Adaptive Dark/Light modes with role-colored messages (User/Assistant/System). Presets include dark, light, high-contrast, and accessible variants.
- **Ranking Modes**: Cycle through `recent`/`balanced`/`relevance`/`quality` with `F12`; quality mode penalizes fuzzy matches.
- **Analytics Dashboard**: 7 views (Dashboard, Explorer, Heatmap, Breakdowns, Tools, Plans, Coverage) with interactive charts, KPI tiles, and drill-down filtering. Toggle with `Alt+A`.
- **Inline Mode**: Run `cass tui --inline` to keep terminal scrollback intact. The UI anchors to a region of the terminal while logs scroll normally. Configure with `--ui-height <rows>` and `--anchor top|bottom`.
- **Macro Recording**: Capture input sessions with `cass tui --record-macro session.macro` for reproducible bug reports and workflow automation. Events are saved as human-readable JSONL with full timing data.
- **Asciicast Recording**: Capture reproducible TUI demos and bug repro artifacts with `cass tui --asciicast demo.cast`.
  - Security default: recording captures terminal output only (input keystrokes are not serialized by default).

### 📄 HTML Session Export

Export conversations as styled, portable HTML files with optional encryption:

- **Mostly Self-Contained**: All layout CSS and the export payload are inlined directly; the file opens without a local web server and references no Tailwind CDN (Tailwind is not used at runtime). Only the Prism.js syntax-highlighting assets are loaded from `cdn.jsdelivr.net`, pinned with SRI hashes.
- **Progressive Enhancement / Graceful Degradation**: Prism.js resources fall back via `onerror="...no-prism"` — code blocks remain readable offline in plain monospace, and the page layout never depends on a network resource.
- **Password Protection**: AES-256-GCM encryption with PBKDF2 key derivation (600,000 iterations)—opens directly in any browser
- **Rich Styling**: Dark/light themes, syntax-highlighted code blocks, collapsible tool calls
- **Print-Friendly**: Optimized print styles with page breaks and footers
- **Searchable**: Built-in search functionality within the exported document

**TUI Usage**: Press `Ctrl+E` in the detail view to open the export modal, or `Ctrl+Shift+E` to export Markdown immediately with defaults. On the detail pane's Export tab, `e`/`h` open the HTML export modal and `m` runs the Markdown export.

**CLI Usage**:
```bash
# Basic export
cass export-html /path/to/session.jsonl

# With encryption
printf '%s\n' "secret" | cass export-html /path/to/session.jsonl --encrypt --password-stdin

# Custom output location
cass export-html session.jsonl --output-dir ~/exports --filename "my-session"

# Open in browser after export
cass export-html session.jsonl --open

# Robot mode (JSON output)
cass export-html session.jsonl --json
```

### 🔗 Universal Connectors
Ingests history from 26 local agents, normalizing them into a unified `Conversation -> Message -> Snippet` model. `cass capabilities --json | jq .connectors` is the canonical machine-readable inventory (kept in lockstep with the runtime registry):
- **Codex**: `~/.codex/sessions` (Rollout JSONL)
- **Cline**: VS Code global storage (Task directories)
- **Gemini CLI**: `~/.gemini/tmp` (Chat JSON)
- **Claude Code**: `~/.claude/projects` (Session JSONL), plus macOS Desktop metadata sidecars under
  `~/Library/Application Support/Claude/claude-code-sessions` and
  `~/Library/Application Support/Claude/local-agent-mode-sessions`
- **Clawdbot**: `~/.clawdbot/sessions` (Session JSONL)
- **Vibe (Mistral)**: `~/.vibe/logs/session/*/messages.jsonl` (Session JSONL)
- **OpenCode**: `.opencode` directories (SQLite)
- **Amp**: `~/.local/share/amp` & VS Code storage
- **Cursor**: `~/Library/Application Support/Cursor/User/` global + workspace storage (SQLite `state.vscdb`)
- **ChatGPT**: `~/Library/Application Support/com.openai.chat` (v1 unencrypted JSON; v2/v3 encrypted—see Environment)
- **Aider**: `~/.aider.chat.history.md` and per-project `.aider.chat.history.md` files (Markdown)
- **Pi-Agent**: `~/.pi/agent/sessions` (Session JSONL with thinking content)
- **Oh My Pi (`omp`)**: OMP v18's default `~/.omp/agent/sessions`, named profiles under `~/.omp/profiles/<name>/agent/sessions`, XDG stores under `$XDG_DATA_HOME/omp`, and explicit OMP-only archive roots via `CASS_OMP_DATA_ROOT` (pi-family JSONL, including per-session sub-agent transcripts)
- **GitHub Copilot Chat**: VS Code global storage under `github.copilot-chat` (JSON)
- **Copilot CLI**: `~/.copilot/session-state`, legacy `~/.copilot/history-session-state`, and `gh copilot` config paths (JSONL/JSON)
- **OpenClaw**: `~/.openclaw/agents/*/sessions` (Session JSONL)
- **Goose**: `~/.local/share/goose/sessions/sessions.db` (SQLite, v1.20+), plus the earlier per-session `*.jsonl` layout under `~/.goose/sessions`
- **Crush**: `~/.crush/crush.db` and per-project `.crush/crush.db` (SQLite)
- **Hermes**: `~/.hermes/state.db` and project-local `.hermes/state.db` (SQLite)
- **Kimi Code**: `$KIMI_CODE_HOME/sessions/*/*/agents/*/wire.jsonl` (default `~/.kimi-code`; sub-agents index as `<sessionId>:<agentId>`), plus the legacy `~/.kimi/sessions/*/*/wire.jsonl` layout (Session JSONL)
- **Muse Code**: `~/.local/share/muse/sessions/<YYYY>/<MM>/<DD>/<session-id>/session.jsonl`, including nested `subagent/*/session.jsonl` transcripts (override with `CASS_MUSE_DATA_ROOT`)
- **Qwen Code**: `~/.qwen/tmp/*/chats/session-*.json` (Chat JSON)
- **Factory (Droid)**: `~/.factory/sessions` (JSONL files organized by workspace slug)
- **Antigravity (agy)**: `~/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs/transcript.jsonl` (clean JSONL transcript), with the durable per-conversation `conversations/<uuid>.db` (SQLite) mirrored alongside. Resume with `cass resume <transcript> --agent agy` (`agy --conversation <uuid>`).
- **OpenHands (OpenDevin)**: `~/.openhands/conversations/<id>/` — `base_state.json` metadata plus an `events/event-NNNNN-<uuid>.json` event stream (JSON)
- **Grok Build (xAI `grok`)**: `~/.grok/sessions/<percent-encoded-cwd>/<session-uuid>/` — `updates.jsonl` (authoritative ACP session-update stream) with `summary.json` metadata and `chat_history.jsonl` fallback (override the base dir with `GROK_HOME`). Resume with `grok --resume <session-id>`.

Claude Code Desktop sidecars preserve title, workspace, model, and session IDs,
but not necessarily the full conversation body. If Claude Code has culled an old
CLI JSONL body, cass can still index searchable sidecar metadata while reporting
that the conversation body is unavailable.

#### Connector Details

**Pi-Agent** parses JSONL session files with rich event structure:
- **Location**: `~/.pi/agent/sessions/` (override the agent home with `PI_CODING_AGENT_DIR`, or the sessions directory directly with `PI_SESSIONS_DIR`)
- **Format**: Typed events—`session_start`, `message`, `model_change`, `thinking_level_change`
- **Features**: Extracts extended thinking content, flattens tool calls with arguments, tracks model changes
- **Detection**: Scans for `*_*.jsonl` pattern in sessions directory

**Oh My Pi (`omp`)** uses the same pi-family wire format but remains a separate
agent identity throughout search, analytics, resume, TUI, and HTML export:
- **Default and profiles**: `~/.omp/agent/sessions/` and `~/.omp/profiles/<name>/agent/sessions/`; `OMP_PROFILE` selects a profile and takes precedence over legacy `PI_PROFILE`
- **XDG**: `$XDG_DATA_HOME/omp/sessions/` and `$XDG_DATA_HOME/omp/profiles/<name>/sessions/` when the OMP XDG root exists
- **Overrides and ownership**: `PI_CODING_AGENT_SESSION_DIR` names the exact OMP sessions directory. `CASS_OMP_DATA_ROOT` declares an OMP-only archive/store root and is the right choice for copied, mounted, or custom OMP data. `PI_CODING_AGENT_DIR` is shared by both pi-family programs, so CASS conservatively keeps otherwise-ambiguous paths under that root owned by Pi-Agent; use one of the OMP-specific variables when OMP identity matters. `PI_CONFIG_DIR` changes the home-relative `.omp` config directory name.
- **Resume**: results in the current live home/config store use `omp [--profile <name>] --resume <id>`; copied profiles, XDG archives, remote mirrors, and explicit roots also carry `--session-dir <dir>` so a canonical-looking archive cannot reopen a different live store
- **Upgrade behavior**: archives created by older cass versions are reclassified from `pi_agent` to `omp` using the same conservative canonical/XDG/remote-mirror ownership policy as live discovery, then the derived lexical index and analytics are rebuilt so a transcript cannot remain attributed to both agents. The conventional `~/.local/share/omp` shape is durable path evidence; an arbitrary historical custom `$XDG_DATA_HOME/omp` path is reclassified only while that root is currently configured and resolvable. Without provider-qualified evidence, ambiguous historical paths fail closed as Pi-Agent rather than letting a generic `.../omp/sessions` directory steal ownership.

**OpenCode** reads SQLite databases from workspace directories:
- **Location**: `.opencode/` directories (scans recursively from home)
- **Format**: SQLite database with sessions table
- **Detection**: Finds directories named `.opencode` containing database files

### 🌐 Remote Sources (Multi-Machine Search)

Search across agent sessions from multiple machines—your laptop, desktop, and remote servers—all from a single unified index. `cass` uses SSH/rsync to efficiently sync session data, tracking provenance so you know where each conversation originated.

#### Interactive Setup Wizard (Recommended)

The easiest way to configure multi-machine search is the interactive setup wizard:

```bash
cass sources setup
```

**What the wizard does:**

1. **Discovers** SSH hosts from your `~/.ssh/config`
2. **Probes** each host to check for:
   - Existing cass installation (and version)
   - Agent session data (Claude, Codex, Cursor, Gemini, etc.)
   - System resources (disk space, memory)
3. **Lets you select** which hosts to configure
4. **Installs cass** on remotes that don't have it (optional)
5. **Indexes** existing sessions on remotes (optional)
6. **Configures** `sources.toml` with correct paths and mappings
7. **Prints the sync command** (`cass sources sync`) for you to run; the wizard does not run the sync itself

**Wizard options:**

| Flag | Purpose |
|------|---------|
| `--hosts <names>` | Configure only specific hosts (comma-separated) |
| `--dry-run` | Preview changes without applying them |
| `--non-interactive` | Use auto-detected defaults for scripting |
| `--skip-install` | Don't install cass on remotes |
| `--skip-index` | Don't run indexing on remotes |
| `--skip-sync` | Skip the final sync phase (which only prints the `cass sources sync` reminder) |
| `--resume` | Resume an interrupted setup |
| `--json` | Output progress as JSON (for automation) |

**Examples:**

```bash
# Full interactive wizard
cass sources setup

# Configure specific hosts only
cass sources setup --hosts css,csd,yto

# Preview without making changes
cass sources setup --dry-run

# Resume interrupted setup
cass sources setup --resume

# Non-interactive for CI/CD
cass sources setup --non-interactive --hosts myserver --skip-install
```

**Resumable state:** If setup is interrupted (Ctrl+C, connection lost), state is saved to the cache directory (`~/.cache/cass/setup_state.json` on Linux). Resume with `--resume`.

#### Remote Installation Methods

When the wizard installs `cass` on remote machines, it chooses one method in this priority order and reports a failure rather than falling through to the next:

| Priority | Method | Speed | Requirements |
|----------|--------|-------|--------------|
| 1 | **cargo-binstall** | ~30s | `cargo-binstall` pre-installed, compatible release binary |
| 2 | **Pre-built binary** | ~10s | curl/wget, GitHub access, compatible release binary |
| 3 | **cargo install** | ~5min | Rust toolchain, 1GB disk, 2GB RAM |
| 4 | **Full bootstrap** | ~10min | curl, 1GB disk, 2GB RAM (installs rustup) |

> **crates.io publishing resumed at 0.7.0 (GH#416):** the long-stale
> registry gap (0.6.13, published before the Quill/OMP era) is closed — the
> entire dependency chain now resolves from crates.io (`frankensearch
> 0.4.0`, the `frankentorch-*` family, `frankenhnsw`), so
> `cargo install coding-agent-search` builds the current line again. The
> installer and GitHub Release binaries remain the fastest paths.

**Resource Requirements**:
- Minimum 1GB disk space for installation
- Recommended 2GB RAM for compilation
- Linux pre-built binaries require glibc 2.38+ on conventional FHS-style distributions; older glibc, musl-only, and NixOS hosts fall back to source installation when possible.
- SSH access with key-based authentication

**What Gets Installed**:
- The `cass` binary (location depends on method: `~/.cargo/bin/cass` for cargo-based, `~/.local/bin/cass` for pre-built binary)
- No daemon, no background services—just the binary

**Installation Progress**: The wizard shows real-time progress for each stage:
```
Installing cass on laptop...
  [1/4] Checking environment...     ✓
  [2/4] Downloading binary...       ████████░░ 80%
  [3/4] Verifying checksum...       ✓
  [4/4] Setting up PATH...          ✓
```

Use `--skip-install` if you prefer to install manually on remotes.

#### Host Discovery & Probing

The setup wizard automatically discovers SSH hosts from your configuration:

**Discovery Sources**:
- `~/.ssh/config` (parses Host entries)
- Hosts with wildcards (`*`, `?`) are automatically excluded

**Probe Results** (for each discovered host):
| Check | Purpose |
|-------|---------|
| **Connectivity** | Can we establish SSH connection? |
| **cass Version** | Is cass already installed? What version? |
| **Agent Data** | Which agents have session data? |
| **Session Count** | How many conversations exist? |
| **System Info** | OS, architecture, disk space, memory |

**Probe Caching**: Results are cached for 5 minutes to speed up repeated setup attempts. Cache clears automatically on expiry.

#### Manual Setup

For manual configuration without the wizard:

```bash
# Add a remote machine using platform presets
cass sources add user@laptop.local --preset macos-defaults

# Or specify paths explicitly
cass sources add dev@workstation --path ~/.claude/projects --path ~/.codex/sessions

# Sync sessions from all configured sources
cass sources sync

# Check source health and connectivity
cass sources doctor
```

#### Remote Archive Safety

Remote source diagnostics are intentionally local-only. `cass triage --json`,
`cass doctor --json`, `cass health --json`, and `cass status --json` report the
`remote_source_sync` summary from cass-owned evidence: `sources.toml`,
`sync_status.json`, the local `remotes/<source>/mirror/` copy, and archive DB
provenance rows. They do not open SSH sessions, mutate remote machines, or
rewrite provider session logs while classifying source gaps.

`cass sources doctor` is the explicit networked exception: it performs bounded,
read-only probes of configured source hosts. Its per-source human summary keeps
the same native reachability, binary-health, and mirror/sync state codes and
safe command as the JSON report. It intentionally does not claim local search
readiness, because a remote host probe cannot establish the controller's local
SQLite, lexical, or semantic asset state.

This matters because agent harnesses can prune their own logs. If a laptop is
retired, a remote path disappears, or a provider truncates older sessions, the
cass archive DB and cass-owned local mirror may be the only remaining evidence
for those conversations. Treat gap names such as `remote_source_unavailable`,
`remote_source_pruned`, `local_archive_ahead_of_remote`, and
`remote_copy_ahead_verified` as preservation signals first: keep the archive and
mirror intact, then run the recommended `cass sources sync --all --json` or
source-specific sync command after reviewing the reported evidence.

Raw-mirror retention is explicit and audited. Use `cass mirror prune
--older-than 90d --json` or `cass mirror prune --max-size 100GB --json` to get a
dry-run plan; add `--apply` only after reviewing the manifest/blob list. Add
`--keep-tag <tag>` to pin captures linked to tagged conversations. `prune`
holds down blobs referenced by captures from the last 7 days by default, writes
`raw-mirror/v1/pruned.jsonl` for every non-empty plan, and refuses apply mode
while an index/watch job is active.

Large mutable sources are stored as 4 MiB content-addressed chunks. Growing
JSONL files reuse every unchanged complete chunk, and SQLite sources reuse
unchanged 4 MiB byte regions, so each historical snapshot remains byte-exact without
writing another full-file blob. Existing whole-blob manifests remain readable;
`cass doctor --json` reports `storage_kind`, `chunk_count`, the full-source
digest, and verifies every referenced chunk before treating a snapshot as
recovery authority.

#### Configuration File

Sources are configured in the platform config directory (Linux: `~/.config/cass/sources.toml`, macOS: `~/Library/Application Support/cass/sources.toml`):

```toml
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects", "~/.codex/sessions"]
sync_schedule = "manual"

[[sources]]
name = "workstation"
type = "ssh"
host = "dev@work.example.com"
paths = ["~/.claude/projects"]
sync_schedule = "daily"

# Path mappings rewrite remote paths to local equivalents
[[sources.path_mappings]]
from = "/home/dev/projects"
to = "/Users/me/projects"

# Agent-specific mappings
[[sources.path_mappings]]
from = "/opt/work"
to = "/Volumes/Work"
agents = ["claude_code"]
```

**Configuration Fields:**
| Field | Description |
|-------|-------------|
| `name` | Friendly identifier (becomes `source_id`) |
| `type` | Connection type: `ssh` or `local` |
| `host` | SSH host (`user@hostname`) |
| `paths` | Paths to sync (supports `~` expansion) |
| `sync_schedule` | `manual`, `hourly`, or `daily` |
| `path_mappings` | Rewrite remote paths to local equivalents |

#### CLI Commands

```bash
# List configured sources
cass sources list [--verbose] [--json]

# Add a new source
cass sources add <user@host> [--name <name>] [--preset macos-defaults|linux-defaults] [--path <path>...] [--no-test]

# Remove a source
cass sources remove <name> [--purge] [-y]

# Check connectivity and config
cass sources doctor [--source <name>] [--json]

# Sync sessions
cass sources sync [--source <name>] [--no-index] [--verbose] [--dry-run] [--json]
```

#### Excluding Noisy Agent Harnesses

If one harness is generating mostly junk or looped output, you can disable it persistently even if its files remain on disk:

```bash
# Inspect current include/exclude state
cass sources agents list --json

# Stop indexing this harness in future runs
cass sources agents exclude openclaw

# Re-enable it later
cass sources agents include openclaw
```

`cass` stores this preference in `sources.toml` (`~/.config/cass/sources.toml` on Linux, `~/Library/Application Support/cass/sources.toml` on macOS), so future scans, syncs, and watch-mode updates remember it automatically.

By default, `cass sources agents exclude <agent>` also removes already archived local data for that agent and rebuilds the lexical index so the exclusion frees space instead of only blocking future imports.

If you want to block future indexing but keep the data already archived:

```bash
cass sources agents exclude openclaw --keep-indexed-data
```

#### Sync Engine Internals

The sync engine uses rsync over SSH for efficient delta transfers, with automatic SFTP fallback:

**Transfer Methods** (auto-detected):
| Method | When Used | Characteristics |
|--------|-----------|-----------------|
| **rsync** | rsync available on both ends | Delta transfers, compression, progress stats |
| **SFTP** | rsync unavailable | Full file transfers via SSH native protocol |

**Safety Guarantees**:
- **Additive-only syncs**: rsync runs WITHOUT `--delete` flag—remote deletions never propagate locally
- **No overwrite risk**: Existing local files are only updated if remote is newer
- **Atomic operations**: Failed transfers don't leave partial files

**Transfer Configuration**:
| Setting | Default | Purpose |
|---------|---------|---------|
| Connection timeout | 10s | Fail fast on unreachable hosts |
| Transfer timeout | 5 min | Allow large initial syncs |
| Compression | Enabled | Reduce bandwidth for text-heavy sessions |
| Partial transfers | Enabled | Resume interrupted syncs |

**rsync Flags Used**:
```
-avz --links --safe-links --stats --partial [--protect-args | --secluded-args] --timeout 300 \
  -e "ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"
```
Where `-avz` = archive mode + verbose + compression. `--protect-args`/`--secluded-args` is auto-detected per remote rsync version (omitted when the remote rejects it), and `--timeout` carries the transfer timeout in seconds.

**Data Flow**:
```
Remote: ~/.claude/projects/
    ↓ (rsync over SSH)
Local: ~/.local/share/coding-agent-search/remotes/<source>/<path>/
    ↓ (connector scan)
Index: agent_search.db + index/v9-quill/
```

Where `<path>` is a filesystem-safe version of the remote path (e.g., `.claude_projects`).

Sessions from remotes are indexed alongside local sessions, with provenance tracking to identify origin.

#### Path Mappings

When viewing sessions from remote machines, workspace paths may not exist locally. Path mappings rewrite these paths so file links work on your local machine:

```bash
# List current mappings
cass sources mappings list laptop

# Add a mapping
cass sources mappings add laptop --from /home/user/projects --to /Users/me/projects

# Test how a path would be rewritten
cass sources mappings test laptop /home/user/projects/myapp/src/main.rs
# Output: /Users/me/projects/myapp/src/main.rs

# Agent-specific mappings (only apply for certain agents)
cass sources mappings add laptop --from /opt/work --to /Volumes/Work --agents claude_code,codex

# Remove a mapping by index
cass sources mappings remove laptop 0
```

#### TUI Source Filtering

In the TUI, filter sessions by origin:
- **F11**: Cycle source filter (all → local → remote → all)
- **Shift+F11**: Open source filter menu to select specific sources

Remote sessions display with a source indicator (e.g., `[laptop]`) in the results list.

#### Provenance Tracking

Each conversation tracks its origin:
- `source_id`: Machine identifier (e.g., "laptop", "workstation")
- `source_kind`: `local` or `remote`
- `workspace_original`: Original path on the remote machine (before path mapping)

These fields appear in JSON/robot output and enable filtering:
```bash
cass search "auth error" --source laptop --json
cass timeline --days 7 --source remote
cass stats --by-source
```

## 🤖 AI / Automation Mode

`cass` is purpose-built for consumption by AI coding agents—not just as an afterthought, but as a first-class design goal. When you're an AI agent working on a codebase, your own session history and those of other agents become an invaluable knowledge base: solutions to similar problems, context about design decisions, debugging approaches that worked, and institutional memory that would otherwise be lost.

### Why Cross-Agent Search Matters

Imagine you're Claude Code working on a React authentication bug. With `cass`, you can instantly search across:
- Your own previous sessions where you solved similar auth issues
- Codex sessions where someone debugged OAuth flows
- Cursor conversations about token refresh patterns
- Aider chats about security best practices

This cross-pollination of knowledge across different AI agents is transformative. Each agent has different strengths, different context windows, and encounters different problems. `cass` unifies all this collective intelligence into a single, searchable index.

### Self-Documenting API

`cass` teaches agents how to use it—no external documentation required:

```bash
# First-stop capability contract for agents
cass triage --json
cass capabilities --json
# → {"version": "...", "workflows": [...], "mistake_recoveries": [...], "commands": [...], "exit_codes": [...], "env_vars": [...]}

# Full API schema with argument types, defaults, and response shapes
cass introspect --json

# Topic-based help optimized for LLM consumption
cass robot-docs commands # All commands and flags
cass robot-docs schemas # Response JSON schemas
cass robot-docs examples # Copy-paste invocations
cass robot-docs exit-codes # Error handling guide
cass robot-docs guide # Quick-start walkthrough
```

### Forgiving Syntax (Agent-Friendly Parsing)

AI agents sometimes make syntax mistakes. `cass` aggressively normalizes input to maximize acceptance when intent is clear:

| What you type | What `cass` understands | Correction note |
|---------------|------------------------|-----------------|
| `cass -robot --limit=5` | `cass --robot --limit=5` | Single-dash long flags normalized |
| `cass --Robot --LIMIT 5` | `cass --robot --limit 5` | Case normalized |
| `cass search "auth" --max_results 5` | `cass search "auth" --limit 5` | Snake-case long flag normalized before alias recovery |
| `cass find "auth"` | `cass search "auth"` | `find`/`query`/`q` → `search` via alias table |
| `cass --robot-docs` | `cass robot-docs` | Flag-as-subcommand detected |
| `cass commands --json` | `cass robot-docs commands` | Robot-docs topic shorthand detected |
| `cass schemas --json` | `cass robot-docs schemas` | Robot-docs topic shorthand detected |
| `cass ready --json` | `cass triage --json` | One-shot triage alias |
| `cass preflight --json` | `cass triage --json` | One-shot triage alias |
| `cass --json` | `cass triage --json` | Top-level robot request defaults to safe preflight |
| `cass --robot` | `cass triage --json` | Top-level robot request defaults to safe preflight |
| `cass --json search "auth"` | `cass search "auth" --json` | Leading structured flag moved to the robot-capable subcommand |
| `cass --robot status` | `cass status --json` | Leading robot flag canonicalized to JSON output |
| `cass answer "auth" --json` | `cass pack "auth" --json` | Cited-handoff aliases normalized to answer pack |
| `cass why auth failed --json --max-evidence 3` | `cass pack "auth failed" --json --max-evidence 3` | Question/RC prompt aliases normalized to answer pack |
| `cass auth failed --json --max-evidence 3` | `cass pack "auth failed" --json --max-evidence 3` | Bare robot queries with pack-only flags become answer packs |
| `cass search auth failed --json --max-evidence 3` | `cass pack "auth failed" --json --max-evidence 3` | Explicit robot search with pack-only flags becomes an answer pack |
| `cass html-export session.jsonl --json` | `cass export-html session.jsonl --json` | Reversed HTML export aliases normalized to the archive exporter |
| `cass current --json` | `cass sessions --current --json` | Current-session shorthand normalized to session discovery |
| `cass sessions current --json` | `cass sessions --current --json` | Positional `current` accepted as the sessions current flag |
| `cass search --query "auth" --json` | `cass search "auth" --json` | Named query option converted to required positional query |
| `cass search --q "auth" --json` | `cass search "auth" --json` | Short/familiar query aliases converted to required positional query |
| `cass search auth error --json` | `cass search "auth error" --json` | Adjacent unquoted query words folded into one search |
| `cass auth error --json` | `cass search "auth error" --json` | Unquoted robot-mode query words folded into search |
| `cass search --agent codex --limit 5 auth error --json` | `cass search "auth error" --agent codex --limit 5 --json` | Query moved before leading search filters |
| `cass view --path session.jsonl --line 42 --json` | `cass view session.jsonl --line 42 --json` | Named path option converted to required positional path |
| `cass view session.jsonl --line-number 42 --json` | `cass view session.jsonl --line 42 --json` | Search result field name accepted as a line alias |
| `cass view session.jsonl line_number=42 --json` | `cass view session.jsonl --line 42 --json` | Search result field assignment accepted as a line option |
| `cass view source_path=session.jsonl source_id=local line_number=42 --json` | `cass view session.jsonl --source local --line 42 --json` | Search hit field bundle accepted as a follow-up command |
| `cass search "auth" --format json` | `cass search "auth" --robot-format json` | Familiar format spelling converted to robot format |
| `cass search "auth" --output json` | `cass search "auth" --robot-format json` | Familiar output spelling converted to robot format |
| `cass help search --json` | `cass robot-docs commands` | Structured help intent routed to the machine-readable command reference |
| `cass --format json status` | `cass status --robot-format json` | Leading format request moved to the target subcommand |
| `cass search "auth" --max-results 5` | `cass search "auth" --limit 5` | Result-count alias converted to canonical limit |
| `cass search "auth" -n 5` | `cass search "auth" --limit 5` | Familiar short count flag converted to canonical limit |
| `cass search "auth" --last 7 --before now` | `cass search "auth" --since -7d --until now` | Familiar time-window aliases converted to canonical filters |
| `cass search "auth" last=7d before=now` | `cass search "auth" --since -7d --until now` | Bare time-window assignments converted to canonical filters |
| `cass search "auth" --provider codex` | `cass search "auth" --agent codex` | Provider/tool/connector aliases converted to canonical agent filter |
| `cass search "auth" provider=codex` | `cass search "auth" --agent codex` | Bare provider assignment converted to canonical agent filter |
| `cass search auth provider codex limit 5` | `cass search auth --agent codex --limit 5` | Bare filter key/value pairs after a query converted to canonical flags |
| `cass search --limt 5` | `cass search --limit 5` | Flag typos within Levenshtein distance ≤2 corrected |

The CLI applies multiple normalization layers:
1. **Flag typo correction**: Long flag names within Levenshtein distance 2 are auto-corrected (e.g. `--limt` → `--limit`). *Subcommand typos are NOT fuzzy-corrected* — use one of the documented aliases instead (see layer 5 below). A typo that isn't a known alias will produce a clap usage error with the canonical form in the hint.
2. **Case normalization**: `--Robot`, `--LIMIT` → `--robot`, `--limit`
3. **Snake-case flag recovery**: `--max_results`, `--data_dir`, and other known snake_case long flags become canonical kebab-case before alias recovery runs
4. **Single-dash recovery**: `-robot` → `--robot` (common LLM mistake)
5. **Subcommand aliases**: `ready`/`preflight` → `triage`; `find`/`query`/`q`/`grep`/`lookup` → `search`; `answer`/`evidence`/`bundle`/`handoff`/`why`/`explain`/`rca`/`root-cause`/`summarize` → `pack`; `html-export`/`html_export`/`exporthtml` → `export-html`; `ls`/`list`/`info`/`summary` → `stats`; `st`/`state` → `status`; `reindex`/`idx`/`rebuild` → `index`; `show`/`get`/`read` → `view`; `docs`/`help-robot`/`robotdocs` → `robot-docs`
6. **Robot-docs topic shorthands**: non-command topics such as `commands`, `schemas`, `examples`, `exit-codes`, and `quickstart` become `robot-docs <topic>` instead of falling through to search; command topics such as `doctor` and `sources` use structured help (`cass help doctor --json`, `cass sources --help --json`). Bare `cass guide` is reserved for the guided-operations planner; use `cass robot-docs guide` for the robot-docs walkthrough.
7. **Root robot default**: `cass --json`, `cass --robot`, or `cass --robot-format json` with no subcommand runs read-only `triage`
8. **Leading structured flag recovery**: `--json`/`--robot` before a robot-capable subcommand is moved onto that subcommand
9. **Named positional recovery**: `--query`/`--q`/`--text`/`--pattern` for search/pack and `--path`/`--source-path`/`--file`/`--session` for drill-down/export commands become the required positional argument
10. **Multi-word query recovery**: adjacent unquoted query words after `search`/`pack` become one query positional
11. **Structured format recovery**: `--format json|jsonl|compact|sessions|toon`, `--output json|jsonl|compact|sessions|toon`, and `--output-format ...` are accepted as `--robot-format ...` on robot-capable commands; `export --format ...` and `export --output <file>` keep their export meanings
12. **Structured help recovery**: `help --json`, `help commands --json`, and `search --help --json` route to `robot-docs guide` / `robot-docs commands`; plain `--help` stays native clap help
13. **Result-count aliases**: `--max-results`, `--num-results`, `--results`, `--count`, `--top-k`, and `-n` become `--limit` on commands with result limits
14. **Time-window aliases**: `--last 7`, `--before now`, `last=7d`, and `before=now` become canonical `--since`/`--until` filters
15. **Provider aliases**: `--provider`, `--tool`, `--connector`, and matching assignments become canonical `--agent` filters on search-like commands
16. **Bare option pairs**: after at least one search/pack query word, `provider codex`, `limit 5`, and `last 7d` become canonical filter flags before the remaining words are folded into the query
17. **Pack-intent recovery**: a bare robot query or explicit structured-output `search` with pack-only flags such as `--max-evidence`, `--max-sessions`, or `--freshness-policy` becomes `pack`, not implicit or explicit `search`
18. **Search-result field aliases**: `--line-number`, `--line_number`, and `line_number=42` become the canonical drill-down `--line` option
19. **Search-hit bundle recovery**: `source_path=... source_id=... line_number=...` can be pasted into follow-up `view`/`expand` commands and becomes the canonical path/source/line form
20. **Leading-filter query recovery**: if a search/pack query comes after leading options, the query is moved back to the required positional slot
21. **Implicit robot search**: unquoted top-level words with an explicit robot/JSON output request become a `search` query unless they look like a subcommand typo
22. **Current-session shorthand**: `current`, `current-session`, and `sessions current` become `sessions --current`
23. **Global flag hoisting**: Position-independent flag handling

When corrections are applied, `cass` emits a teaching note to stderr so agents learn the canonical syntax. In robot/JSON mode the same information is emitted as one `note: auto-corrected: <note>` line per correction on stderr, so stdout stays data-only.

### Structured Output Formats

Every command supports machine-readable output:

```bash
# Pretty-printed JSON (default robot mode)
cass search "error" --robot

# Streaming JSONL: one hit per line. Add --robot-meta to prepend a
# _meta header line (elapsed_ms, next_cursor, state, index_freshness).
cass search "error" --robot-format jsonl               # hits only
cass search "error" --robot-format jsonl --robot-meta  # 1 _meta header + hits

# Compact single-line JSON (minimal bytes)
cass search "error" --robot-format compact

# Include performance metadata
cass search "error" --robot --robot-meta
# → { "hits": [...], "_meta": { "elapsed_ms": 12, "cache_hit": true, "wildcard_fallback": false, "lexical_degrade_reason": null, ... } }
#   lexical_degrade_reason is "query_fuel_exhausted" when a hybrid search dropped its
#   lexical leg because Quill's query fuel ran out (see CASS_QUILL_QUERY_FUEL_BUDGET)

# Per-hit trust verdict (advisory; --robot-meta only)
cass search "error" --robot --robot-meta
# Each hit then carries a metadata-only `trust` block:
#   "trust": {
#     "schema_version": 1,
#     "trust_tier": "unverified",     // trusted | likely | unverified | stale | failed
#     "confidence": "medium",         // low | medium | high
#     "provenance_refs": [],          // e.g. ["commit:ab0d12ef90ab", "bead:xyz", "release:v0.6.15"]
#     "stale_reason": "aged_out",     // present only when not fully trusted
#     "recommended_followup": "..."   // advisory next step (never a destructive command)
#   }
```

**How agents should branch on `trust_tier`** (relevance is not correctness — a
hit can be a landed fix or a failed attempt):

| `trust_tier` | Meaning | What to do |
|--------------|---------|------------|
| `trusted` | Landed, proof-backed, release/bead-contained | Safe to reuse |
| `likely` | Has provenance (commit/closed bead) but not proof-pinned | Confirm via the cited ref first |
| `unverified` | Relevant but no provenance link, or lexical-only corroboration | Corroborate before reuse |
| `stale` | Aged out (`aged_out`) or superseded (`superseded_by_newer`) | Prefer a newer result |
| `failed` | A failed/reverted attempt (`failed_attempt`) | Do not reuse |

The verdict is **advisory metadata only** — it never changes result ordering.
It is derived from metadata-only signals (recency, source health, realized
search mode, cwd-relative workspace match, and — opportunistically — linked
commit/bead/release provenance); it carries no raw session text. The same
`trust` block is attached to `cass pack` evidence. Branch on `trust_tier` and
`stale_reason`, not on `confidence` alone.

Provenance correlation is **project-scoped and explicit-reference anchored**:
for a hit from the project you are running `cass` in now, cass links it to a
closed bead / commit / proof / release only when the hit's own indexed text
references a known identifier (`bead:<id>`, `commit:<sha>`, `release:<tag>`),
joined against that project's local beads and git history. A temporal or
workspace coincidence is never enough, so an unrelated conversation never
inherits another's trust. Off-project hits report `workspace_mismatch`, and a
hit whose local source file no longer exists on disk reports `source_unhealthy`
(archive-only) instead of overtrusting a dead path.

```bash
# Deterministic answer pack for handoff prompts
cass pack "why did checkout fail" --robot --max-tokens 12000 --limit 40

# Freshness-sensitive pack: fail if selected evidence is outside the window
cass pack "checkout timeout after redirect" --robot \
  --freshness-policy strict --freshness-window-seconds 604800 \
  --max-tokens 12000 --require-evidence

# Token-budgeted pack for pasting into another agent
cass pack "checkout timeout after redirect" --robot \
  --max-tokens 4000 --max-evidence 8 --max-sessions 3 --max-excerpt-chars 600

# Pipeline from broad search to a bounded cited handoff
cass search "checkout timeout" --robot-format sessions \
  | cass pack "checkout timeout root cause" --robot --sessions-from -
```

**Design principle**: stdout contains only parseable JSON data; all diagnostics, warnings, and progress go to stderr.

Use `search` when you are still exploring candidate sessions. Use `pack` when
you need a compact, cited, extractive artifact to hand to another agent or a
human operator. Use `status`/`health` before trusting freshness-sensitive output,
and use `doctor` only for diagnostics or safe repair workflows. Use
`export-html` when you need a full browsable session archive; packs are
token-budgeted evidence bundles, not full exports and not external
summarization.

Pack robot output includes `health`, `freshness`, `privacy`, and `warnings`.
Warnings such as `privacy_redactions_applied`, `semantic_fallback_lexical`,
or `no_evidence_found` are data, not prose; branch on the JSON fields before
copying the pack into another tool. Stale selected evidence is structural:
inspect `freshness.stale_evidence_count`.

### Swarm Operations Workflow

Use the swarm surfaces when multiple agents are sharing one repo and you need a
single read-only view before claiming work:

```bash
# Current shared-work snapshot; does not claim, reopen, release, or run builds
cass swarm status --json

# Advisory packet for one bead; still create real reservations and Beads updates yourself
cass swarm work-packet --json --bead coding_agent_session_search-example

# Coordination hygiene check before closeout or takeover review
cass swarm lint --json --bead coding_agent_session_search-example

# Read-only sibling dependency drift sentinel
cass swarm dependency-drift --json
```

`swarm status`, `swarm work-packet`, and `swarm lint` currently compose their
snapshot from checked-in fixtures (`--fixture <file>` or `--fixture-dir <dir>
--fixture-id <id>`); without a fixture the live provider path reports every
source as `live-provider-unimplemented`. Only `swarm dependency-drift` has a
live path today.

`swarm status` composes Beads, Agent Mail metadata, git state, rch/build
pressure, cass health/status, and proof references. Stale candidates are
advisory only: coordinate through Beads and Agent Mail before reopening,
force-releasing, or taking over work. Suggested commands are robot-safe
templates, not automatic actions.

`swarm dependency-drift` reads `Cargo.toml` and optional sibling checkouts to
report manifest pins, local HEAD/dirty state, strict validation commands, and
release-risk recommendations. It does not fetch remotes, edit manifests, run
builds, update Beads, send Agent Mail, delete files, or mutate git state.

When status points at prior evidence, use `cass pack "query" --robot` to create
a bounded cited handoff for another agent. Packs complement the cockpit; they do
not replace Beads for ownership, Agent Mail for coordination, or rch for proof
commands.

### Token Budget Management

LLMs have context limits. `cass` provides multiple levers to control output size:

| Flag | Effect |
|------|--------|
| `--fields minimal` | Only `source_path`, `line_number`, `agent` |
| `--fields summary` | Adds `title`, `score` |
| `--fields score,title,snippet` | Custom field selection |
| `--max-content-length 500` | Truncate long fields (UTF-8 safe, adds "...") |
| `--max-tokens 2000` | Soft budget (~4 chars/token); adjusts truncation dynamically |
| `--limit 5` | Cap number of results |
| `cass pack "query" --robot` | Build a cited handoff pack from selected search evidence |
| `pack --max-tokens N` | Set the pack planner's soft budget |
| `pack --max-evidence N` | Cap evidence items selected into the pack |
| `pack --max-sessions N` | Limit how many sessions can contribute evidence |
| `pack --max-excerpt-chars N` | Shorten each cited excerpt before token estimation |
| `pack --fields summary` | Return top-level summary fields for a smaller JSON envelope |
| `pack --freshness-policy strict --freshness-window-seconds N` | Reject stale evidence instead of silently mixing it into a pack |
| `pack --sessions-from FILE` | Restrict pack evidence to newline-delimited session paths; use `-` for stdin |

Truncated fields include a `*_truncated: true` indicator so agents know when they're seeing partial content.

Contributor verification for docs or contract changes should use `rch`, for example:

```bash
rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_answer_pack_docs \
  cargo test --test golden_robot_docs
```

### Error Handling for Agents

Errors are structured, actionable, and include recovery hints. A real sample from `cass search foo --robot` against a fresh data dir:

```json
{
  "error": {
    "code": 3,
    "kind": "missing-index",
    "message": "cass has not been initialized in <data_dir> yet, so search cannot run until the first index completes.",
    "hint": "Run 'cass index --full' once to discover local sessions and build the initial archive.",
    "retryable": true
  }
}
```

**Kind names** are kebab-case (e.g. `missing-index`, `missing-db`, `semantic-unavailable`, `embedder-unavailable`, `ambiguous-source`, `timeout`, `config`, `lock-busy`, `network`). Agents that branch on `err.kind` should treat them as stable identifiers. The full set (~50 kinds as of 0.3.x) is defined in `src/lib.rs`; the canonical way to discover a kind programmatically is to trigger the condition and inspect `err.kind` from the JSON envelope.

**Exit codes** follow a semantic convention:
| Code | Meaning | Typical action |
|------|---------|----------------|
| 0 | Success | Parse stdout |
| 1 | Health check failed | Run `cass index --full` |
| 2 | Usage error | Fix syntax (hint provided) |
| 3 | Index/DB missing | Run `cass index --full` (retryable: true) |
| 4 | Network error | Check connectivity |
| 5 | Data corruption | Run `cass doctor check --json`; repair or restore the canonical SQLite archive before indexing |
| 6 | Incompatible version | Update cass |
| 7 | Lock/busy | Retry later |
| 8 | Partial result (`sources sync` only: some sources had path failures) | Inspect per-path errors in the JSON output and retry the failed sources |
| 9 | Unknown error | Check `retryable` flag |
| 10 | Config / timeout | Depends on `err.kind` |
| 11 | Config validation | Fix config |
| 12 | Source / SSH | Check remote host |
| 13 | Mapping / not-found | Depends on `err.kind` |
| 14 | I/O / mapping | Retry or inspect path |
| 15 | Semantic / embedder unavailable | Install model or `--mode lexical` |
| 20-21 | Model acquisition | Check `err.kind`, `err.hint` |
| 22 | I/O during model handling | Retry |
| 23 | Model download | Retry or use `--from-file` |
| 24 | I/O during model verify/install | Retry |

Search/pack timeouts are not exit 8: on expiry `search` and `pack` exit 0 with `{"hits": [], "budget": {"timed_out": true, "skipped_sections": [...], "retry": "<command>", ...}}`, and `--robot-format sessions` instead fails with exit 10, kind `timeout`.

**Codes ≥ 10 are domain-specific** and the numeric value alone is ambiguous (e.g. code 10 maps to either `config` or `timeout` kinds depending on context). Agents should branch on `err.kind` from the JSON error envelope — not on the numeric code — when handling codes ≥ 10. See the Error Handling section above for the canonical `kind` list.

The `retryable` field tells agents whether a retry might succeed (e.g., transient I/O) vs. guaranteed failure (e.g., invalid path).

### Session Analysis Commands

Beyond search, `cass` provides commands for deep-diving into specific sessions:

```bash
# Discover the current session for this workspace
cass sessions --current --json

# List recent sessions for a specific project
cass sessions --workspace /path/to/project --json --limit 5

# Export full conversation to shareable format
cass export /path/to/session.jsonl --format markdown -o conversation.md
cass export /path/to/session.jsonl --format json --include-tools

# Export as self-contained HTML with encryption (recommended for sharing)
cass export-html /path/to/session.jsonl                     # To Downloads folder
printf '%s\n' "pwd" | cass export-html session.jsonl --encrypt --password-stdin
cass export-html session.jsonl --open --json                # Open in browser, JSON output

# Common agent flow: find current session, then export it
cass export-html "$(cass sessions --current --json | jq -r '.sessions[0].path')" --json

# Expand context around a specific line (from search result)
cass expand /path/to/session.jsonl -n 42 -C 5 --json
# → Shows 5 messages before and after line 42

# Activity timeline: when were agents active?
cass timeline --today --json --group-by hour
cass timeline --since 7d --agent claude --json
# → Grouped activity counts, useful for understanding work patterns
```

### Aggregation & Analytics

Aggregate search results server-side to get counts and distributions without transferring full result data:

```bash
# Count results by agent
cass search "error" --robot --aggregate agent
# → { "aggregations": { "agent": { "buckets": [{"key": "claude_code", "count": 45}, ...] } } }

# Multi-field aggregation
cass search "bug" --robot --aggregate agent,workspace,date

# Combine with filters
cass search "TODO" --agent claude --robot --aggregate workspace
```

**Aggregation Fields**:
| Field | Description |
|-------|-------------|
| `agent` | Group by agent type (claude_code, codex, cursor, etc.) |
| `workspace` | Group by workspace/project path |
| `date` | Group by date (YYYY-MM-DD) |
| `match_type` | Group by match quality (exact, prefix, fuzzy) |

**Response Format**:
```json
{
  "aggregations": {
    "agent": {
      "buckets": [
        {"key": "claude_code", "count": 120},
        {"key": "codex", "count": 85}
      ],
      "other_count": 15
    }
  }
}
```

Top 10 buckets are returned per field, with `other_count` for remaining items.

#### Bounded incident mining

Mine recurrent CASS operational incidents from the canonical archive without
dumping raw session text:

```bash
cass analytics incidents --limit 10 --json

# Tighten the bounded scan for automation or a very large archive
cass analytics incidents --max-sessions 500 --max-messages 50000 \
  --max-bytes 67108864 --budget-ms 5000 --json
```

The response ranks `top_sessions[]` by hit count and category breadth and keeps
the exact `conversation_id`, agent, host, `source_id`, `source_path`,
live/archive state, dominant categories, and a structured `cass view` argv.
That argv carries the effective `--db` path plus `--conversation-id`, so it
opens the exact ranked archive row even when multiple sessions share a source
path or the report used a non-default database.
`total_sessions`, `total_hits`, and `top_sessions_truncated` distinguish the
bounded ranked result from the totals observed inside the scan scope.
`discovery.partial` and `stop_reason` explicitly distinguish a bounded partial
scan from a complete scan. Counts are scoped to scanned candidates whenever the
scan is partial. `--budget-ms` is a hard wall-clock result guard around the
independently row-bounded read-only worker. If it expires before a verified
result arrives, `count_scope="no_verified_results_hard_timeout"` returns an
empty partial report instead of overstating in-flight observations. Candidate
discovery is descending archive-row keyset paging;
`--max-sessions` bounds that newest-row window before dimensional filters, so a
selective filter can truthfully return a partial empty result instead of scanning
an unbounded archive. Individual messages are inspected through a bounded 4,096-char
fragment; an oversized message returns `message-fragment-capped` rather than
claiming a complete corpus scan. Raw prompt/tool content is always suppressed; evidence carries
only BLAKE3 fingerprints and basename-redacted paths. The actionable
`source_path` remains visible solely so the returned view command works.

### Chained Search (Pipeline Mode)

Chain multiple searches together by piping session paths from one search to another:

```bash
# Find sessions mentioning "auth", then search within those for "token"
cass search "authentication" --robot-format sessions | \
  cass search "refresh token" --sessions-from - --robot

# Build a filtered corpus from today's work
cass search --today --robot-format sessions > today_sessions.txt
cass search "bug fix" --sessions-from today_sessions.txt --robot
```

**How It Works**:
1. First search with `--robot-format sessions` outputs one session path per line
2. Second search with `--sessions-from <file>` restricts search to those sessions
3. Use `-` to read from stdin for true piping

**Use Cases**:
- **Drill-down**: Broad search → narrow within results
- **Cross-reference**: Find sessions with term A, then find term B within them
- **Corpus building**: Save session lists for repeated searches

### Match Highlighting

The `--highlight` flag wraps matching terms for visual/programmatic identification:

```bash
cass search "authentication error" --robot --highlight
# In text output: **authentication** and **error** are bold-wrapped
# In HTML export: <mark>authentication</mark> and <mark>error</mark>
```

Highlighting is query-aware: quoted phrases like `"auth error"` highlight as a unit; individual terms highlight separately.

### Pagination & Cursors

For large result sets, use cursor-based pagination:

```bash
# First page
cass search "TODO" --robot --robot-meta --limit 20
# → { "hits": [...], "_meta": { "next_cursor": "eyJ..." } }

# Next page
cass search "TODO" --robot --robot-meta --limit 20 --cursor "eyJ..."
```

Cursors are opaque tokens encoding the pagination state. They remain valid as long as the index isn't rebuilt.

### Request Correlation

For debugging and logging, attach a request ID:

```bash
cass search "bug" --robot --request-id "req-12345"
# → { "hits": [...], "_meta": { "request_id": "req-12345" } }
```

### Idempotent Operations

For safe retries (e.g., in CI pipelines or flaky networks):

```bash
cass index --full --idempotency-key "build-$(date +%Y%m%d)"
# If same key + params were used in last 24h, returns cached result
```

### Query Analysis

Debug why a search returned unexpected results:

```bash
cass search "auth*" --robot --explain
# → Includes parsed query AST, term expansion, cost estimates

cass search "auth error" --robot --dry-run
# → Validates query syntax without executing
```

### Traceability

For debugging agent pipelines:

```bash
cass search "error" --robot --trace-file /tmp/cass-trace.json
# Appends execution span with timing, exit code, and command details

cass index --full --json --robot-trace-ingest 2>/tmp/cass-ingest-trace.jsonl
# Streams one NDJSON record per ingest batch with wall_ms, batch_msgs,
# inserted_messages, and duplicate-lookup counters for perf bisects
```

### Search Flags Reference

| Flag | Purpose |
|------|---------|
| `--robot` / `--json` | JSON output (pretty-printed) |
| `--robot-format jsonl\|compact` | Streaming or single-line JSON |
| `--robot-meta` | Include `_meta` block (elapsed_ms, cache stats, index freshness, `lexical_degrade_reason`: `"query_fuel_exhausted"` or null) |
| `--fields minimal\|summary\|<list>` | Reduce payload size |
| `--max-content-length N` | Truncate content fields to N chars |
| `--max-tokens N` | Apply an approximate token budget to robot output |
| `--timeout N` | Timeout in milliseconds. On expiry `search`/`pack` still exit 0 and emit `{"hits": [], "budget": {"timed_out": true, "skipped_sections": [...], "retry": "<command>", ...}}`; `--robot-format sessions` fails with exit 10, kind `timeout` |
| `--cursor <token>` | Cursor-based pagination (from `_meta.next_cursor`) |
| `--request-id ID` | Echoed in response for correlation |
| `--aggregate agent,workspace,date` | Server-side aggregations |
| `--explain` | Include query analysis (parsed query, cost estimate) |
| `--dry-run` | Validate query without executing |
| `--no-maintenance` | Strict read-only search: never refresh, join, or spawn lexical maintenance, never auto-repair the archive while opening it, and never auto-spawn the daemon (conflicts with `--refresh` and `--daemon`) |
| `--source <source>` | Filter by source: `local`, `remote`, `all`, or specific source ID |
| `--highlight` | Highlight matching terms in output |

### Index Flags Reference

| Flag | Purpose |
|------|---------|
| `--idempotency-key KEY` | Safe retries: same key + params returns cached result (24h TTL) |
| `--json` | JSON output with stats |

### Robot Documentation System

For machine-readable documentation, use `cass robot-docs <topic>`:

| Topic | Content |
|-------|---------|
| `commands` | Full command reference with all flags |
| `env` | Environment variables and defaults |
| `paths` | Data directory locations per platform |
| `guide` | Quick start guide for automation |
| `schemas` | JSON response schemas |
| `exit-codes` | Exit code meanings and retry guidance |
| `examples` | Copy-paste usage examples |
| `contracts` | API contract version and stability |
| `sources` | Remote sources configuration guide |

```bash
# Get documentation programmatically
cass robot-docs guide
cass robot-docs schemas
cass robot-docs exit-codes

# Machine-first help (wide output, no TUI assumptions)
cass --robot-help
```

### API Contract & Versioning

`cass` maintains a stable API contract for automation:

```bash
cass api-version --json
# → { "crate_version": "<cargo version>", "build_commit": "<sha or unknown>", "api_version": 1, "contract_version": "1" }

cass introspect --json
# → Full schema: all commands, arguments, response types
```

**Contract Version**: Currently `1`. Increments only on breaking changes.

**Guaranteed Stable**:
- Exit codes and their meanings
- JSON response structure for `--robot` output
- Flag names and behaviors
- `_meta` block format

### Ready-to-paste blurb for AGENTS.md / CLAUDE.md

```
🔎 cass — Search All Your Agent History

 What: cass indexes conversations from Claude Code, Codex, Cursor, Gemini, Aider, ChatGPT, and more into a unified, searchable index. Before solving a problem from scratch, check if any agent already solved something similar.

 ⚠️ NEVER run bare cass — it launches an interactive TUI. Always use --robot or --json.

 Quick Start

 # One-shot agent triage (read next_command when present)
 cass triage --json

 # Search across all agent histories
 cass search "authentication error" --robot --limit 5

 # Build a cited handoff pack from search evidence
 cass pack "authentication error root cause" --robot --max-tokens 12000 --limit 40

 # Tight handoff budget with freshness and privacy metadata
 cass pack "authentication error root cause" --robot --max-tokens 4000 --max-evidence 8 --fields summary

 # View a specific result (from search output)
 cass view /path/to/session.jsonl -n 42 --json

 # Expand context around a line
 cass expand /path/to/session.jsonl -n 42 -C 3 --json

 # Learn the full API
 cass capabilities --json # Static agent self-description
 cass robot-docs guide # LLM-optimized docs

 Why Use It

 - Cross-agent knowledge: Find solutions from Codex when using Claude, or vice versa
 - Forgiving syntax: Typos and wrong flags are auto-corrected with teaching notes
 - Token-efficient: --fields minimal returns only essential data; pack budgets cite only selected evidence
 - Copy-safe handoffs: pack warnings include freshness and privacy/redaction status

 Key Flags

 | Flag | Purpose |
 |------------------|--------------------------------------------------------|
 | --robot / --json | Machine-readable JSON output (required!) |
 | --fields minimal | Reduce payload: source_path, line_number, agent only |
 | pack --max-tokens N | Budget a cited handoff pack |
 | --limit N | Cap result count |
 | --agent NAME | Filter to specific agent (claude, codex, cursor, etc.) |
 | --days N | Limit to recent N days |

 stdout = data only, stderr = diagnostics. Exit 0 = success.
```

---

## 🔤 Query Language Reference

`cass` supports a rich query syntax designed for both humans and machines.

### Basic Queries

| Query | Matches |
|-------|---------|
| `error` | Messages containing "error" (case-insensitive) |
| `python error` | Messages containing both "python" AND "error" |
| `"authentication failed"` | Exact phrase match |
| `auth fail` | Both terms, in any order |

### Boolean Operators

Combine terms with explicit operators for complex queries:

| Operator | Example | Meaning |
|----------|---------|---------|
| `AND` | `python AND error` | Both terms required (default) |
| `OR` | `error OR warning` | Either term matches |
| `NOT` | `error NOT test` | First term, excluding second |
| `-` | `error -test` | Shorthand for NOT |

**Operator Precedence**: NOT binds tightest, then AND, then OR. Use parentheses (in robot mode) for explicit grouping.

```bash
# Complex boolean query
cass search "authentication AND (error OR failure) NOT test" --robot

# Exclude test files
cass search "bug fix -test -spec" --robot

# Either error type
cass search "TypeError OR ValueError" --robot
```

### Phrase Queries

Wrap terms in double quotes for exact phrase matching:

| Query | Matches |
|-------|---------|
| `"file not found"` | Exact sequence "file not found" |
| `"cannot read property"` | Exact JavaScript error message |
| `"def test_"` | Function definitions starting with test_ |

Phrases respect word order and proximity. Useful for error messages, code patterns, and specific terminology.

### Wildcard Patterns

| Pattern | Type | Matches | Performance |
|---------|------|---------|-------------|
| `auth*` | Prefix | "auth", "authentication", "authorize" | Fast (uses edge n-grams) |
| `*tion` | Suffix | "authentication", "function", "exception" | Slower (regex scan) |
| `*config*` | Substring | "reconfigure", "config.json", "misconfigured" | Slowest (full regex) |
| `test_*` | Prefix | "test_user", "test_auth", "test_helpers" | Fast |

**Tip**: Prefix wildcards (`foo*`) are optimized via pre-computed edge n-grams. Suffix and substring wildcards fall back to regex and are slower on large indexes.

### Query Modifiers

```bash
# Field-specific search (in robot mode)
cass search "error" --agent claude --workspace /path/to/project

# Time-bounded search
cass search "bug" --since 2024-01-01 --until 2024-01-31
cass search "bug" --today
cass search "bug" --days 7

# Combined filters
cass search "authentication" --agent codex --workspace myproject --week
```

### Flexible Time Input

`cass` accepts a wide variety of time/date formats for filtering:

| Format | Examples | Description |
|--------|----------|-------------|
| **Relative** | `-7d`, `-24h`, `-30m`, `-1w` | Days, hours, minutes, weeks ago |
| **Keywords** | `now`, `today`, `yesterday` | Named reference points |
| **ISO 8601** | `2024-11-25`, `2024-11-25T14:30:00Z` | Standard datetime |
| **US Dates** | `11/25/2024`, `11-25-2024` | Month/Day/Year |
| **Unix Timestamp** | `1732579200` | Seconds since epoch |
| **Unix Millis** | `1732579200000` | Milliseconds (auto-detected) |

**Intelligent Heuristics**:
- Numbers >10 digits are treated as milliseconds, otherwise seconds
- Two-digit years are expanded (24 → 2024)
- Date-only inputs default to midnight start or 23:59:59 end

```bash
# All equivalent for "last week"
cass search "bug" --since -7d
cass search "bug" --since "-1w"
cass search "bug" --days 7

# Date range
cass search "feature" --since 2024-01-01 --until 2024-01-31

# Mix formats
cass search "error" --since yesterday --until now
```

### Match Types

Search results include a `match_type` indicator:

| Type | Meaning | Score Boost |
|------|---------|-------------|
| `exact` | Query terms found verbatim | Highest |
| `prefix` | Matched via prefix expansion (e.g., `auth*`) | High |
| `suffix` | Matched via suffix pattern | Medium |
| `substring` | Matched via substring pattern | Lower |
| `fuzzy` | Auto-fallback match when exact results sparse | Lowest |

### Auto-Fuzzy Fallback

When an exact query returns fewer than 3 results, `cass` automatically retries with wildcard expansion:
- `auth` → `*auth*`
- Results are flagged with `wildcard_fallback: true` in robot mode
- TUI shows a "fuzzy" indicator in the status bar

---

## ⌨️ Complete Keyboard Reference

### Global Keys

| Key | Action |
|-----|--------|
| `Ctrl+C` | Force quit |
| `Esc` / `F10` | Unwind: close the open modal or surface, otherwise quit |
| `F1` / `Alt+?` | Toggle help screen |
| `F2` / `Alt+T` | Next theme (cycles all 19 presets) |
| `Shift+F2` / `Alt+Shift+T` | Previous theme |
| `Ctrl+B` | Toggle border style (rounded/plain) |
| `Ctrl+P` / `Alt+P` | Open the command palette |
| `Ctrl+S` | Toggle the stats bar |
| `Ctrl+Shift+S` | Open the sources management surface |
| `Alt+A` | Open the analytics dashboard |
| `Alt+M` | Toggle macro recording (replay with `cass tui --play-macro FILE`) |
| `Ctrl+Shift+I` | Toggle the inspector overlay |
| `Ctrl+Shift+R` | Force re-index |
| `Ctrl+Shift+Del` | Reset all TUI state |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |

Launch-time flags: `cass tui --refresh` (alias `--catch-up`) runs an incremental index pass before opening; `--record-macro FILE` / `--play-macro FILE` record and replay input events.

### Search Bar (Query Input)

| Key | Action |
|-----|--------|
| Type | Live search as you type; plain characters (including `?`, `y`, `o`, `c`, `1`-`9`, `-`, `=`) go into the query |
| `Enter` | Open the selected hit; with no selected hit, submit the query (if the query is empty, edit the last filter chip) |
| `Backspace` | Delete character; if the query is empty, remove the last filter chip |
| `Left`/`Right`, `Ctrl+Left`/`Ctrl+Right` | Move the cursor by character / by word |
| `Home`/`End` | Jump the cursor to the start / end of the query |
| `Ctrl+L` | Clear the query |
| `Ctrl+U` / `Ctrl+K` / `Ctrl+W` | Kill to line start / to line end / previous word |
| `Ctrl+R` | Cycle through query history |
| `Ctrl+N` / `Ctrl+Shift+N` | Next / previous query-history entry |
| `Ctrl+F` | Toggle wildcard fallback |
| `Ctrl+Shift+Y` | Copy the query |

### Navigation

| Key | Action |
|-----|--------|
| `Up`/`Down` | Move selection in results list |
| `PageUp`/`PageDown` | Scroll by page |
| `Tab` / `Shift+Tab` | Toggle focus between results and detail pane / move focus left |
| `Alt+h/j/k/l` | Vim-style directional focus (left/down/up/right) |
| `Alt+1`..`Alt+9` | Switch to pane N |
| `Alt+-` / `Alt+=` | Shrink / grow the results pane |
| `Alt+D` | Hide / show the detail pane |
| `Alt+[` / `Alt+]` | Timeline jump backward / forward |

### Filtering

| Key | Action |
|-----|--------|
| `F3` / `Alt+G` | Open agent filter palette |
| `Shift+F3` / `Alt+Shift+G` | Clear the agent filter |
| `F4` / `Alt+W` | Open workspace filter palette |
| `Shift+F4` / `Alt+Shift+W` / `Ctrl+Del` | Clear all active filters |
| `F5` | Set "from" time filter |
| `F6` | Set "to" time filter |
| `Shift+F5` | Cycle time presets: 24h → 7d → 30d → all |
| `F11` / `Shift+F11` | Cycle the source filter / open the source filter menu |
| `Alt+/` | Open the pane filter |

### Modes & Display

| Key | Action |
|-----|--------|
| `F7` / `Alt+C` | Cycle context window size: S → M → L → XL |
| `Ctrl+Space` | Momentary "peek" to XL context |
| `F9` | Toggle match mode: prefix (default) ↔ standard |
| `F12` / `Alt+R` | Cycle ranking: recent → balanced → relevance → quality → newest → oldest |
| `Alt+S` | Cycle search mode (lexical / semantic / hybrid) |
| `Ctrl+D` | Cycle density: Compact → Cozy → Spacious |
| `Ctrl+1`..`Ctrl+9` | Save the current view to slot N |
| `Shift+1`..`Shift+9` | Load the view from slot N |

### Selection & Actions

| Key | Action |
|-----|--------|
| `Enter` / `Ctrl+M` | Open selected result in the detail modal (Messages tab by default) |
| `Ctrl+X` | Toggle selection on current result |
| `Ctrl+A` | Select/deselect all visible results |
| `Alt+B` | Open bulk actions menu (when items selected) |
| `Ctrl+Enter` | Add to multi-open queue |
| `Ctrl+O` | Open all queued items in editor |
| `F8` / `Alt+O` | Open selected hit in `$EDITOR` |
| `Alt+V` | View raw |
| `Alt+Shift+J` | Toggle JSON view |
| `Ctrl+Y` | Copy path |
| `Alt+Y` | Copy snippet |
| `Ctrl+Shift+C` | Copy content |
| `Ctrl+E` | Open the export modal |
| `Ctrl+Shift+E` | Export Markdown immediately |
| `Alt+U` / `Alt+N` / `Alt+I` | Update banner: upgrade now / show release notes / skip this version |

### Detail Pane

These apply while the detail modal is open:

| Key | Action |
|-----|--------|
| `Esc` | Close the detail modal |
| `Tab` | Cycle detail tabs |
| `/` (or `Ctrl+F`, `Alt+/`) | Start find-in-detail; type to search, `Enter` advances to the next match |
| `n` / `N` | Next / previous contextual search hit within this session |
| `Enter` (Messages tab) | Next contextual search hit |
| `j` / `k`, `Up`/`Down` | Scroll |
| `g` / `G`, `Home`/`End` | Scroll to top / bottom |
| `{` / `}` | Jump to previous / next message |
| `[` / `]` | Jump to previous / next user message |
| `w` | Toggle line wrap |
| `e` / `c` | Expand / collapse all tool and system messages |
| `e`, `h` (Export tab) | Open the HTML export modal; `m` exports Markdown |
| `F7` | Cycle context window size |
| `Ctrl+Space` | Momentary "peek" to XL context |

### Detail Tabs

The detail pane has six tabs, cycled with `Tab`:

| Tab | Content | Best For |
|-----|---------|----------|
| **Messages** | Full conversation with markdown rendering | Reading full context |
| **Snippets** | Keyword-extracted summaries | Quick scanning |
| **Raw** | Unformatted JSON/text | Debugging, copying exact content |
| **Json** | Syntax-highlighted JSON with a collapsible tree | Inspecting structured payloads |
| **Analytics** | Per-session token timeline, tool calls, message stats | Understanding one session |
| **Export** | Export actions and filename previews (HTML/Markdown) | Sharing a session |

### Context Window Sizing

Control how much content shows in the detail preview. Cycle with `F7`:

| Size | Characters | Use Case |
|------|------------|----------|
| **Small** | ~200 | Quick scanning, narrow terminals |
| **Medium** | ~400 | Default balanced view |
| **Large** | ~800 | Reading longer passages |
| **XLarge** | ~1600 | Full context, code review |

**Peek Mode** (`Ctrl+Space`): Temporarily expand to XL context. Press again to restore previous size. Useful for quick deep-dives without changing your preferred default.

### Mouse Support

- **Click** on result to select
- **Click** on filter chip to edit/remove
- **Scroll** in any pane
- **Double-click** to open result

### Bulk Operations

Efficiently work with multiple search results at once:

**Multi-Select Mode**:
1. Press `Ctrl+X` to toggle selection on current result (checkbox appears)
2. Navigate to other results and press `Ctrl+X` again
3. Press `Ctrl+A` to select/deselect all visible results
4. Selected count shown in footer: "3 selected"

**Bulk Actions Menu** (`Alt+B` when items selected):
| Action | Description |
|--------|-------------|
| **Open All** | Open all selected files in editor |
| **Copy Paths** | Copy all file paths to clipboard |
| **Export** | Export selected results to file |
| **Clear Selection** | Deselect all items |

**Multi-Open Queue**:
For opening many files without navigating away:
1. Press `Ctrl+Enter` to add current result to queue
2. Continue searching and adding more results
3. Press `Ctrl+O` to open all queued items
4. Confirmation prompt appears for 12+ items

**Clipboard Operations**:
- `Ctrl+Y` - Copy the current item's path
- `Alt+Y` - Copy the current item's snippet
- `Ctrl+Shift+C` - Copy the current item's content
- Bulk actions menu → **Copy Paths** for every selected item

---

## 📊 Ranking & Scoring Explained

### The Six Ranking Modes

Cycle through modes with `F12`:

1. **Recent Heavy** (default): Strongly favors recent conversations
   - Score = `text_relevance × 0.3 + recency × 0.7`
   - Best for: "What was I working on?"

2. **Balanced**: Equal weight to relevance and recency
   - Score = `text_relevance × 0.5 + recency × 0.5`
   - Best for: General-purpose search

3. **Relevance**: Prioritizes text match quality
   - Score = `text_relevance × 0.8 + recency × 0.2`
   - Best for: "Find the best explanation of X"

4. **Match Quality**: Penalizes fuzzy/wildcard matches
   - Score = `text_relevance × 0.7 + recency × 0.2 + match_exactness × 0.1`
   - Best for: Precise technical searches

5. **Date Newest**: Pure chronological order (newest first)
   - Ignores relevance scoring entirely
   - Best for: "Show me all recent activity"

6. **Date Oldest**: Pure reverse chronological order (oldest first)
   - Ignores relevance scoring entirely
   - Best for: "When did I first work on this?"

### Score Components

- **Text Relevance (BM25)**: Quill's implementation of Okapi BM25, considering:
  - Term frequency in document
  - Inverse document frequency across corpus
  - Document length normalization

- **Recency**: Exponential decay from current time
  - Documents from today: ~1.0
  - Documents from last week: ~0.7
  - Documents from last month: ~0.3

- **Match Exactness**: Bonus for exact matches vs wildcards
  - Exact phrase: 1.0
  - Prefix match: 0.8
  - Suffix/Substring: 0.5
  - Fuzzy fallback: 0.3

### Blended Scoring Formula

The final score combines all components using mode-specific weights:

```
Final_Score = BM25_Score × Match_Quality + α × Recency_Factor
```

**Alpha (α) by Ranking Mode**:
| Mode | α Value | Effect |
|------|---------|--------|
| Recent Heavy | 1.0 | Recency dominates |
| Balanced | 0.4 | Moderate recency boost |
| Relevance Heavy | 0.1 | BM25 dominates |
| Match Quality | 0.0 | Pure text matching |
| Date Newest/Oldest | N/A | Pure chronological sort |

**Match Quality Factors**:
| Match Type | Factor | Applied When |
|------------|--------|--------------|
| Exact | 1.0 | `"exact phrase"` |
| Prefix | 0.9 | `auth*` |
| Suffix | 0.8 | `*tion` |
| Substring | 0.6 | `*config*` |
| Implicit Wildcard | 0.4 | Auto-fallback expansion |

**Recency Factor**: `timestamp / max_timestamp` normalized to [0, 1].

This formula ensures that "Recent Heavy" mode (default) surfaces your most recent work, while "Relevance Heavy" finds the best explanations regardless of age.

---

## 🔄 The Normalization Pipeline

Each connector transforms agent-specific formats into a unified schema:

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Agent Files    │ ──▶ │    Connector     │ ──▶ │  Normalized     │
│  (proprietary)  │     │  (per-agent)     │     │  Conversation   │
└─────────────────┘     └──────────────────┘     └─────────────────┘
     JSONL                   detect()                agent_slug
     SQLite                  scan()                  workspace
     Markdown                                        messages[]
     JSON                                            created_at
```

### Role Normalization

Different agents use different role names:

| Agent | Original | Normalized |
|-------|----------|------------|
| Claude Code | `human`, `assistant` | `user`, `assistant` |
| Codex | `user`, `assistant` | `user`, `assistant` |
| ChatGPT | `user`, `assistant`, `system` | `user`, `assistant`, `system` |
| Cursor | `user`, `assistant` | `user`, `assistant` |
| Aider | (markdown headers) | `user`, `assistant` |

### Timestamp Handling

Agents store timestamps inconsistently:

| Format | Example | Handling |
|--------|---------|----------|
| Unix milliseconds | `1699900000000` | Direct conversion |
| Unix seconds | `1699900000` | Multiply by 1000 |
| ISO 8601 | `2024-01-15T10:30:00Z` | Parse with chrono |
| Missing | `null` | Use file modification time |

### Content Flattening

Tool calls, code blocks, and nested structures are flattened for searchability:

```json
// Original (Claude Code)
{"type": "tool_use", "name": "Read", "input": {"path": "/foo/bar.rs"}}

// Flattened for indexing
"[Tool: Read] path=/foo/bar.rs"
```

---

## 🧹 Deduplication Strategy

The same conversation content can appear multiple times due to:
- Agent file rewrites
- Backup files
- Symlinked directories
- Re-indexing

### Content-Based Deduplication

`cass` uses a multi-layer deduplication strategy:

1. **Message identity**: messages are keyed by `UNIQUE(conversation_id, idx)` and inserted with `INSERT OR IGNORE`, so re-indexing the same file never stores a message twice
   - No content hash is persisted for this; BLAKE3 content hashes are computed in memory only, as merge fingerprints when an updated file is reconciled against stored rows

2. **Conversation identity**: conversations are keyed by `UNIQUE(source_id, agent_id, external_id)`
   - There is no fingerprint built from message hashes; the same external id from the same source and agent is the same conversation

3. **Search-Time Dedup**: hits are deduplicated on an exact key tuple — `(source, source path, conversation id or title, line number, created_at, whitespace-invariant content hash)` — keeping the highest-scored hit
   - Identical content from different sources stays visible as separate results; tool-invocation noise is filtered

### Noise Filtering

Common low-value content is filtered from results:
- Empty messages
- Pure whitespace
- System prompts (unless searching for them)
- Repeated tool acknowledgments

---

## 💼 Use Cases & Workflows

### 1. "I solved this before..."

```bash
# Find past solutions for similar errors
cass search "TypeError: Cannot read property" --days 30

# In TUI: F12 to switch to "relevance" mode for best matches
```

### 2. Cross-Agent Knowledge Transfer

```bash
# What has ANY agent said about authentication in this project?
cass search "authentication" --workspace /path/to/project

# Export findings for a new agent's context
cass export /path/to/relevant/session.jsonl --format markdown
```

### 3. Daily/Weekly Review

```bash
# What did I work on today?
cass timeline --today --json | jq '.groups[].conversations'

# TUI: Press Shift+F5 to cycle through time filters
```

### 4. Debugging Workflow Archaeology

```bash
# Find all debugging sessions for a specific file
cass search "debug src/auth/login.rs" --agent claude

# Expand context around a specific line in a session
cass expand /path/to/session.jsonl -n 150 -C 10
```

### 5. Agent-to-Agent Handoff

```bash
# Current agent searches what previous agents learned
cass search "database migration strategy" --robot --fields minimal

# Get full context for a relevant session
cass view /path/to/session.jsonl -n 42 --json
```

### 6. Building Training Data

```bash
# Export high-quality problem-solving sessions
cass search "bug fix" --robot --limit 100 | \
  jq '.hits[] | select(.score > 0.8)' > training_candidates.json
```

---

## 🎯 Command Palette

Press `Ctrl+P` to open the command palette—a fuzzy-searchable menu of all available actions.

### Available Commands

| Command | Description |
|---------|-------------|
| Toggle theme | Switch between dark/light mode |
| Toggle density | Cycle Compact → Cozy → Spacious |
| Toggle help strip | Pin/unpin the contextual help bar |
| Check updates | Show update assistant banner |
| Filter: agent | Open agent filter picker |
| Filter: workspace | Open workspace filter picker |
| Filter: today | Restrict results to today |
| Filter: last 7 days | Restrict results to past week |
| Filter: date range | Prompt for custom since/until |
| Saved views | List and manage saved view slots |
| Save view to slot N | Save current filters to slot 1-9 |
| Load view from slot N | Restore filters from slot 1-9 |
| Bulk actions | Open bulk menu (when items selected) |
| Reload index/view | Refresh the search reader |

### Usage

1. Press `Ctrl+P` to open
2. Type to fuzzy-filter commands
3. Use `Up`/`Down` to navigate
4. Press `Enter` to execute
5. Press `Esc` to close

---

## 💾 Saved Views

Save your current filter configuration to one of 9 slots for instant recall.

### What Gets Saved

- Active filters (agent, workspace, time range)
- Current ranking mode
- The search query

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Ctrl+1` through `Ctrl+9` | Save current view to slot |
| `Shift+1` through `Shift+9` | Load view from slot |

### Via Command Palette

1. `Ctrl+P` → "Save view to slot N"
2. `Ctrl+P` → "Load view from slot N"
3. `Ctrl+P` → "Saved views" to list all slots

### Persistence

Views are stored in `tui_state.json` and persist across sessions. Clear all saved views with `Ctrl+Shift+Del` (resets all TUI state).

---

## 📐 Density Modes

Control how many lines each search result occupies. Cycle with `Ctrl+D` or via the command palette.

| Mode | Lines per Result | Best For |
|------|------------------|----------|
| **Compact** | 2 | Maximum results visible, scanning many items |
| **Cozy** (default) | 5 | Balanced view with context |
| **Spacious** | 6 | Detailed preview, fewer results |

The pane automatically adjusts how many results fit based on terminal height and density mode.

---

## 🎨 Theme System

`cass` includes a sophisticated theming system with multiple presets, accessibility-aware color choices, and adaptive styling.

### Theme Presets

Cycle through 19 built-in theme presets with `F2`:

| Theme | Description | Best For |
|-------|-------------|----------|
| **Tokyo Night** (default) | Deep blues with restrained contrast | Low-light environments, extended sessions |
| **Daylight** | High-contrast light background | Bright environments, presentations |
| **Catppuccin Mocha** | Warm pastels, reduced eye strain | All-day coding, aesthetic preference |
| **Dracula** | Purple-accented dark theme | Popular among developers, familiar feel |
| **Nord** | Arctic-inspired cool tones | Calm, focused work sessions |
| **Solarized Dark** | Precisely tuned low-contrast palette | Long editing sessions, monitor-agnostic |
| **Solarized Light** | Solarized on a cream background | Paper-style readability in bright rooms |
| **Monokai** | Classic warm dark palette | Familiar Sublime/TextMate feel |
| **Gruvbox Dark** | Retro earth tones on dark | Warmer alternative to Tokyo Night |
| **One Dark** | Atom's signature balanced dark | Moderate contrast, friendly defaults |
| **Rosé Pine** | Soho-inspired muted roses | Gentle contrast, boutique look |
| **Everforest** | Forest-inspired green-brown palette | Calm, nature-adjacent mood |
| **Kanagawa** | Japanese ink-and-paper theme | Artistic, quietly distinctive |
| **Ayu Mirage** | Ayu's balanced muted dark | Blue-teal accents, relaxed contrast |
| **Nightfox** | Fox-inspired warm dark | Deep violets with orange highlights |
| **Cyberpunk Aurora** | Neon aurora on obsidian | Showy, high-saturation dark |
| **Synthwave '84** | Retro neon magenta/cyan | 80s aesthetic, fun demos |
| **High Contrast** | Maximum readability | Accessibility needs, bright monitors |
| **Colorblind** | Deuteranopia/protanopia-safe palette | Color-vision-deficient users |

### WCAG Accessibility

All theme colors are validated against WCAG (Web Content Accessibility Guidelines) contrast requirements:

- **Text on backgrounds**: Minimum 4.5:1 contrast ratio (AA standard)
- **Large text/headers**: Minimum 3:1 contrast ratio
- **Interactive elements**: Clear visual distinction from content

The theming engine calculates relative luminance and contrast ratios at runtime to ensure readability across all color combinations.

### Role-Aware Message Styling

Conversation messages are color-coded by role for quick visual parsing:

| Role | Visual Treatment | Purpose |
|------|------------------|---------|
| **User** | Blue-tinted background, bold | Your input, easy to scan |
| **Assistant** | Green-tinted background | AI responses |
| **System** | Gray/muted background | Context, instructions |
| **Tool** | Orange-tinted background | Tool calls, file operations |

Each agent type (Claude, Codex, Cursor, etc.) also receives a subtle tint, making multi-agent result lists instantly scannable.

### Adaptive Borders

Border decorations automatically adapt to terminal width:

| Width | Style | Example |
|-------|-------|---------|
| **Narrow** (<80 cols) | Minimal Unicode | `│ content │` |
| **Normal** (80-120) | Rounded corners | `╭─ content ─╮` |
| **Wide** (>120) | Full decorations | Double-line headers |

Toggle between rounded Unicode and plain ASCII borders with `Ctrl+B`.

---

## 🔖 Bookmark System

Bookmarks are a CLI feature: `cass bookmarks add|list|remove|search|export|import --json` manages user-authored annotations on search results (a source path, optional line number, note, and tags). The TUI has no bookmark keybindings today.

```bash
# Bookmark a search hit (source_path + line_number from search output)
cass bookmarks add /path/to/session.jsonl -n 42 --title "JWT refresh fix" \
  --note "Good explanation of the refresh flow" --tags "auth,jwt" --json

# List (optionally by tag), search notes/titles/snippets, remove by id
cass bookmarks list --tag auth --json
cass bookmarks search "refresh" --json
cass bookmarks remove 1 --json          # exit 13 (`bookmark-not-found`) if the id is unknown

# Back up and restore
cass bookmarks export -o bookmarks.json --json
cass bookmarks import bookmarks.json --json
```

### Features

- **Persistent storage**: Bookmarks saved to `bookmarks.db` (SQLite), separate from the search index and never pruned by doctor/cleanup flows
- **Notes**: Add annotations explaining why you bookmarked something
- **Tags**: Organize with comma-separated tags (e.g., "rust, important, auth"); `list` can filter by tag
- **Search**: Find bookmarks by title, note, or snippet content
- **Export/Import**: JSON format for backup and sharing

### Bookmark Structure

```json
{
  "id": 1,
  "title": "Auth bug fix discussion",
  "source_path": "/path/to/session.jsonl",
  "line_number": 42,
  "agent": "claude_code",
  "workspace": "/projects/myapp",
  "note": "Good explanation of JWT refresh flow",
  "tags": "auth, jwt, important",
  "snippet": "The token refresh logic should..."
}
```

### Storage Location

Bookmarks are stored separately from the main index:
- Linux: `~/.local/share/coding-agent-search/bookmarks.db`
- macOS: `~/Library/Application Support/coding-agent-search/bookmarks.db`
- Windows: `%APPDATA%\coding-agent-search\bookmarks.db`

---

## 🔔 Toast Notification System

`cass` uses a non-intrusive toast notification system for transient feedback—operations complete, errors occur, or state changes without modal dialogs interrupting your workflow.

### Notification Types

| Type | Icon | Auto-Dismiss | Use Case |
|------|------|--------------|----------|
| **Info** | ℹ️ | 3 seconds | Status updates, tips |
| **Success** | ✓ | 2 seconds | Operations completed |
| **Warning** | ⚠ | 4 seconds | Non-critical issues |
| **Error** | ✗ | 6 seconds | Failures requiring attention |

### Behavior

- **Non-Blocking**: Toasts appear in a corner without stealing focus
- **Auto-Dismiss**: Each type has an appropriate display duration
- **Message Coalescing**: Duplicate messages show a count badge instead of stacking
- **Configurable Position**: Toasts can appear in any corner (default: top-right)
- **Maximum Visible**: Limited to 3-5 visible toasts to prevent screen clutter

### Visual Design

Toasts feature:
- **Color-coded borders**: Matches notification type (blue/green/yellow/red)
- **Theme-aware**: Adapts to current dark/light theme
- **Subtle animation**: Fade in/out for smooth appearance

### Common Toast Messages

| Trigger | Toast |
|---------|-------|
| Index rebuild complete | ✓ "Index rebuilt: 2,500 conversations" |
| Export complete | ✓ "Exported to conversation.md" |
| Copy to clipboard | ✓ "Copied to clipboard" |
| Search timeout | ⚠ "Search timed out, showing partial results" |
| Connector error | ✗ "Failed to scan ChatGPT: encrypted files" |
| Update available | ℹ️ "Version 0.5.0 available" |

---

## 🏎️ Performance Engineering: Caching & Warming
To achieve sub-60ms latency on large datasets, `cass` implements a multi-tier caching strategy in `src/search/query.rs`:

1. **Sharded LRU Cache**: The `prefix_cache` is split into shards (default 256 entries each) to reduce mutex contention during concurrent reads/writes from the async searcher.
2. **Bloom Filter Pre-checks**: Each cached hit stores a 64-bit Bloom filter mask of its content tokens. When a user types more characters, we check the mask first. If the new token isn't in the mask, we reject the cache entry immediately without a string comparison.
3. **Predictive Warming**: A background `WarmJob` thread watches the input. When the user pauses typing, it triggers a lightweight query against the lexical reader to pre-load relevant index segments into the OS page cache.

## 🔌 The Connector Interface (Polymorphism)
The system is designed for extensibility via the `Connector` trait (`src/connectors/mod.rs`). This allows `cass` to treat disparate log formats as a uniform stream of events.

```mermaid
classDiagram
 class Connector {
 <<interface>>
 +detect() DetectionResult
 +scan(ScanContext) Vec~NormalizedConversation~
 }
 class NormalizedConversation {
 +agent_slug String
 +messages Vec~NormalizedMessage~
 }

 Connector <|-- CodexConnector
 Connector <|-- ClineConnector
 Connector <|-- ClaudeCodeConnector
 Connector <|-- GeminiConnector
 Connector <|-- ClawdbotConnector
 Connector <|-- VibeConnector
 Connector <|-- OpenCodeConnector
 Connector <|-- AmpConnector
 Connector <|-- CursorConnector
 Connector <|-- ChatGptConnector
 Connector <|-- AiderConnector
 Connector <|-- PiAgentConnector
 Connector <|-- FactoryConnector
 Connector <|-- CopilotConnector
 Connector <|-- CopilotCliConnector
 Connector <|-- OpenClawConnector
 Connector <|-- CrushConnector
 Connector <|-- HermesConnector
 Connector <|-- KimiConnector
 Connector <|-- QwenConnector

 CodexConnector ..> NormalizedConversation : emits
 ClineConnector ..> NormalizedConversation : emits
 ClaudeCodeConnector ..> NormalizedConversation : emits
 GeminiConnector ..> NormalizedConversation : emits
 ClawdbotConnector ..> NormalizedConversation : emits
 VibeConnector ..> NormalizedConversation : emits
 OpenCodeConnector ..> NormalizedConversation : emits
 AmpConnector ..> NormalizedConversation : emits
 CursorConnector ..> NormalizedConversation : emits
 ChatGptConnector ..> NormalizedConversation : emits
 AiderConnector ..> NormalizedConversation : emits
 PiAgentConnector ..> NormalizedConversation : emits
 FactoryConnector ..> NormalizedConversation : emits
 CopilotConnector ..> NormalizedConversation : emits
 CopilotCliConnector ..> NormalizedConversation : emits
 OpenClawConnector ..> NormalizedConversation : emits
 CrushConnector ..> NormalizedConversation : emits
 HermesConnector ..> NormalizedConversation : emits
 KimiConnector ..> NormalizedConversation : emits
 QwenConnector ..> NormalizedConversation : emits
```

- **Polymorphic Scanning**: The indexer runs connector factories in parallel via rayon, creating fresh `Box<dyn Connector>` instances that are unaware of each other's underlying file formats (JSONL, SQLite, specialized JSON).
- **Resilient Parsing**: Connectors handle legacy formats (e.g., integer vs ISO timestamps) and flatten complex tool-use blocks into searchable text.

---

## 🧠 Architecture & Engineering

`cass` uses frankensqlite as the durable source of truth and frankensearch as a derived speed layer, powered by a suite of integrated "franken" libraries.

### The Pipeline
1. **Discovery**: [franken_agent_detection](https://github.com/Dicklesworthstone/franken_agent_detection) auto-discovers sessions from 26 coding agents (Claude Code, Codex, Cursor, Gemini, Aider, Amp, Cline, OpenCode, ChatGPT, Pi Agent, Oh My Pi, Copilot, Copilot CLI, OpenClaw, Clawdbot, Vibe, Crush, Goose, Hermes, Kimi, Muse Code, Qwen, Factory, OpenHands, Antigravity, Grok Build).
2. **Storage (frankensqlite)**: The **Source of Truth**. Data is persisted to a normalized SQLite schema (`messages`, `conversations`, `agents`) via [frankensqlite](https://github.com/Dicklesworthstone/frankensqlite) — a pure-Rust SQLite reimplementation. Production writes use single-writer `BEGIN IMMEDIATE` transactions; an experimental opt-in parallel persist path (`CASS_INDEXER_BEGIN_CONCURRENT=1`, off by default) exists but is not the default.
3. **Search Index (frankensearch)**: The **Speed Layer**. New messages are incrementally pushed to a unified search index via [frankensearch](https://github.com/Dicklesworthstone/frankensearch) which provides BM25 lexical search, semantic embeddings, RRF fusion, and cross-encoder reranking in a single library.
 * **Fields**: `title`, `content`, `agent`, `workspace`, `created_at`.
 * **Prefix Fields**: `title_prefix` and `content_prefix` use **Index-Time Edge N-Grams** (not stored on disk to save space) for instant prefix matching.
 * **Deduping**: Search results are deduplicated on an exact key tuple (source, source path, conversation, line number, timestamp, whitespace-invariant content hash) and tool-invocation noise is filtered.

```mermaid
flowchart LR
 classDef pastel fill:#f4f2ff,stroke:#c2b5ff,color:#2e2963;
 classDef pastel2 fill:#e6f7ff,stroke:#9bd5f5,color:#0f3a4d;
 classDef pastel3 fill:#e8fff3,stroke:#9fe3c5,color:#0f3d28;
 classDef pastel4 fill:#fff7e6,stroke:#f2c27f,color:#4d350f;
 classDef pastel5 fill:#ffeef2,stroke:#f5b0c2,color:#4d1f2c;

 subgraph Sources["Local Sources"]
 A1[Codex]:::pastel
 A2[Cline]:::pastel
 A3[Gemini]:::pastel
 A4[Claude]:::pastel
 A5[OpenCode]:::pastel
 A6[Amp]:::pastel
 A7[Cursor]:::pastel
 A8[ChatGPT]:::pastel
 A9[Aider]:::pastel
 A10[Pi-Agent]:::pastel
 A11[Factory]:::pastel
 A12[Copilot Chat]:::pastel
 A13[Copilot CLI]:::pastel
 A14[OpenClaw]:::pastel
 A15[Clawdbot]:::pastel
 A16[Vibe]:::pastel
 A17[Crush]:::pastel
 A18[Hermes]:::pastel
 A19[Kimi]:::pastel
 A20[Qwen]:::pastel
 end

 subgraph Remote["Remote Sources"]
 R1["sources.toml"]:::pastel
 R2["SSH/rsync\nSync Engine"]:::pastel2
 R3["remotes/\nSynced Data"]:::pastel3
 end

 subgraph "Ingestion Layer"
 C1["franken_agent_detection\nAuto-Discover & Scan\nNormalize & Dedupe"]:::pastel2
 end

 subgraph "Storage + Search"
 S1["frankensqlite (WAL)\nSource of Truth\nBEGIN IMMEDIATE\nMigrations"]:::pastel3
 T1["frankensearch\nBM25 + Semantic\nRRF Fusion\nReranking"]:::pastel4
 end

 subgraph "Presentation"
 U1["TUI (FrankenTUI)\nElm Architecture\nAnalytics Dashboard\nAsync Search"]:::pastel5
 U2["CLI / Robot\nJSON Output\nAutomation"]:::pastel5
 end

 A1 --> C1
 A2 --> C1
 A3 --> C1
 A4 --> C1
 A5 --> C1
 A6 --> C1
 A7 --> C1
 A8 --> C1
 A9 --> C1
 A10 --> C1
 A11 --> C1
 A12 --> C1
 A13 --> C1
 A14 --> C1
 A15 --> C1
 A16 --> C1
 A17 --> C1
 A18 --> C1
 A19 --> C1
 A20 --> C1
 R1 --> R2
 R2 --> R3
 R3 --> C1
 C1 -->|Persist| S1
 C1 -->|Index| T1
 S1 -.->|Rebuild| T1
 T1 -->|Query| U1
 T1 -->|Query| U2
```

### Background Indexing & Watch Mode
- **Non-Blocking**: The indexer runs in a background thread. You can search while it works.
- **Parallel Discovery**: Connector detection and scanning run in parallel across all CPU cores using rayon, significantly reducing startup time when multiple agents are installed.
- **Watch Mode** (`cass index --watch`, foreground): Uses file system watchers (`notify`) to detect changes in agent logs. When you save a file or an agent replies, `cass` re-indexes just that conversation. The TUI does **not** start a watcher on its own; see *Keeping the Index Fresh* below for what runs automatically.
- **Real-Time Progress**: The TUI footer updates in real-time showing discovered agent count and conversation totals with sparkline visualization (e.g., "📦 Indexing 150/2000 (7%) ▁▂▄▆█").

### Keeping the Index Fresh (Automatic)

An index that is always a little behind is the most common complaint about any local search tool, so cass has three cooperating mechanisms. None of them block a search; all of them run `cass index --background`, which lowers its own CPU (`nice 15`) and I/O (`ionice` idle on Linux) priority before touching anything, and all of them respect the single `index-run.lock` — two indexers never run at once.

| Layer | What | When it runs | Enable |
|-------|------|--------------|--------|
| **Stale-on-read catch-up** | `search`, `pack`, and TUI launch check index freshness. If the index is stale (> 30 min), partial, or has pending sessions, a *detached* incremental `cass index --background` is spawned in its own process group and the current results are returned immediately. The next search is fresh. | On demand, at most once per 5 min per data dir (`CASS_AUTO_REFRESH_COOLDOWN_SECS`). Never for data dirs under the OS temp dir, and never for `search --no-maintenance`. | On by default. `CASS_AUTO_REFRESH=0` disables globally. `--robot-meta` reports `index_freshness.auto_refresh.{outcome,trigger,pid}`. |
| **OS scheduler** (`cass schedule install`) | launchd LaunchAgents (macOS) or systemd user timers (Linux): an **incremental** job every 15 min and a **nightly** job (03:00) that runs `index --full`, then bounded `models backfill --scheduled` batches (fast/hash tier always; quality/MiniLM tier when the model is installed), plus any remote-source syncs whose `sync_schedule` in `sources.toml` is due. Priority is delegated to the OS (`ProcessType=Background`/`Nice`/`LowPriorityIO`, `Nice=19`/`IOSchedulingClass=idle`/`CPUSchedulingPolicy=idle`). | On the timer, even when no cass process is running; survives reboots (`Persistent=true` / launchd). | `cass schedule install [--interval-mins 15] [--nightly-hour 3] [--no-nightly] [--no-semantic] [--dry-run]`; `cass schedule status`; `cass schedule uninstall`. |
| **Resident daemon timer** | The warm-model daemon (`cass daemon`, auto-spawned by semantic/hybrid searches) can also kick an incremental background index while it is resident. | Every `CASS_DAEMON_INDEX_INTERVAL_SECS` seconds while the daemon lives (it exits after its idle timeout). | Off by default; `CASS_DAEMON_INDEX_INTERVAL_SECS=900` recommended. |

Idle awareness: scheduled work skips a run when the machine is under severe load (Linux `/proc/loadavg` + PSI; macOS `sysctl vm.loadavg`). On macOS you can additionally require the console to have been idle — `CASS_RESPONSIVENESS_MIN_USER_IDLE_SECS=600` makes the nightly job and scheduled semantic backfill wait until nobody has touched the keyboard for ten minutes (the gate fails open where idle time is unavailable). Foreground `cass index` is never gated.

Everything a scheduled job did is recorded under `<data_dir>/schedule/` (`state.json`, `runs.jsonl`, per-job logs) and the last stale-on-read spawn under `<data_dir>/auto-refresh-state.json` / `auto-refresh.log`; `cass schedule status --json` reads all of it.

```bash
# See what would be registered, then register it
cass schedule install --dry-run
cass schedule install

# Run a job by hand (what the units invoke); --force ignores load/idle gates
cass schedule run --job incremental --json
cass schedule run --job nightly --force

# Inspect
cass schedule status --json
cass search "auth" --robot --robot-meta | jq '._meta.index_freshness.auto_refresh'
```

## 🔍 Deep Dive: Internals

### The TUI Engine (Elm Architecture on FrankenTUI)
The interactive interface (`src/ui/app.rs`) uses **FrankenTUI (ftui)**, a Rust TUI framework implementing the Elm architecture (Model-View-Update). The runtime handles terminal lifecycle, event polling, rendering, and cleanup.

1. **Model (CassApp)**: A monolithic struct tracks the entire UI state (search query, cursor position, scroll offsets, active filters, cached details, animation state).
2. **Update**: Each event (key, mouse, tick, resize) maps to a `CassMsg` variant. The `update()` function produces `Cmd` effects (async tasks, ticks, quit).
3. **View**: The `view()` function renders the current state to an ftui `Frame`. The runtime diff engine minimizes terminal writes using Bayesian strategy selection.
4. **Adaptive Budget**: A 16ms (60fps) frame budget with PID-controlled degradation automatically simplifies rendering (borders, animations) when frame times exceed budget.
5. **Background Tasks**: Search queries, indexing, and analytics run on background threads via `Cmd::Task`, with results delivered as messages.

```mermaid
graph TD
 Input([User Input]) -->|Key/Mouse/Tick| Runtime
 Runtime -->|CassMsg| Update[Model::update]
 Update -->|Cmd| Runtime
 Update -->|State Change| View[Model::view]
 View -->|Frame| DiffEngine[Bayesian Diff]
 DiffEngine -->|Minimal Writes| Terminal

 Update -->|Cmd::Task| Background[Background Thread]
 Background -->|Result Msg| Runtime
```

### Append-Only Storage Strategy
Data integrity is paramount. `cass` treats the SQLite database (`src/storage/sqlite.rs`, powered by frankensqlite) as an **append-only log** for conversations:

- **Immutable History**: When an agent adds a message to a conversation, we don't update the existing row. We insert the new message linked to the conversation ID.
- **Deduplication**: Messages are keyed by `UNIQUE(conversation_id, idx)` and inserted with `INSERT OR IGNORE`, so an agent re-writing a file cannot store a message twice; BLAKE3 content hashes are used only in memory as merge fingerprints.
- **Versioning**: A `_schema_migrations` table and strict migration path (20 versioned migrations at HEAD; see *Database Schema Migrations*) ensure that upgrades are safe and atomic.

---

## 🛡️ Index Resilience & Recovery

`cass` treats search indexes as derived assets. The SQLite archive is authoritative; lexical and semantic search data can be rebuilt from it.

### Schema Version Tracking

Every lexical generation stores a `schema_hash.json` file containing the schema fingerprint:

```json
{"schema_hash":"quill-fslx-schema-v9-hyphen-cjk-bigrams-bounded-content-prefix-preview-stored-content-external"}
```

### Automatic Recovery Scenarios

| Scenario | Detection | Recovery |
|----------|-----------|----------|
| First run | No SQLite archive and no lexical index | `cass index --full` discovers sessions and creates both |
| Missing lexical index | No readable lexical asset | Rebuild from SQLite into scratch space, then publish |
| Schema mismatch | Hash differs from current | Rebuild derived lexical asset from SQLite |
| Corrupted metadata | Invalid or missing lexical metadata | Ignore the broken derivative and rebuild from SQLite |
| Semantic not ready | Model/vector assets absent or still backfilling | Continue lexical search and report semantic fallback/readiness |

### Manual Recovery

```bash
# Check the current truth surface first
cass triage --json
cass health --json
cass status --json

# If not ready, run the first targeted command from recommended_commands[].
# For a fresh data dir this is usually:
cass index --full --json --no-progress-events --data-dir <same-data-dir>
```

Manual rebuild commands are for first setup, explicit operator refresh, or cases where `recommended_commands[]` asks for them. A normal missing/stale lexical asset should be repaired as derived state from SQLite, not treated as lost user data.

### Design Principles

1. **Never lose source data**: `cass` only reads agent files, never modifies them
2. **SQLite is the source of truth**: Derived lexical and semantic assets can be rebuilt
3. **Atomic publish**: Rebuilt assets are prepared in scratch space and published only when complete
4. **Graceful degradation**: Hybrid search continues as lexical when semantic enrichment is unavailable

### Index Recovery & Self-Healing

`cass` maintains multiple layers of redundancy to recover from corruption or schema changes:

**Schema Hash Versioning**:
Each lexical generation stores a `schema_hash.json` file containing a hash of the current schema definition. On startup:
1. If hash matches → open existing index
2. If hash differs → schema changed, trigger rebuild
3. If file missing/corrupted → assume stale, trigger rebuild

This ensures that version upgrades with schema changes can rebuild the lexical derivative without user intervention.

**Automatic Rebuild Triggers**:
| Condition | Detection | Action |
|-----------|-----------|--------|
| Schema version change | Hash mismatch in `schema_hash.json` | Full rebuild |
| Missing Quill publication manifest | Quill can't open index | Rebuild and publish a fresh derivative |
| Corrupted index files | Lexical reader open fails | Rebuild and publish a fresh derivative |
| Explicit request | `--force-rebuild` flag | Rebuild derived search assets from the canonical SQLite archive |

**SQLite as Ground Truth**:
The SQLite database serves as the authoritative data store. Lexical rebuilds reconstruct the Quill index from SQLite:
```rust
// Iterate all conversations from SQLite
// Re-index each message into a fresh Quill index
// Progress tracked via IndexingProgress for UI feedback
```

This means corrupted lexical data is a repairable derivative-state problem. Operators should start with `cass triage --json` for the exact next command, or read `cass health --json` / `cass status --json` for the narrower readiness snapshot.

### Database Schema Migrations

The SQLite database uses 20 versioned schema migrations, tracked in the `_schema_migrations` table (`CURRENT_SCHEMA_VERSION = 20` and `MIGRATION_NAMES` in `src/storage/sqlite.rs`):

| Version | Migration | Version | Migration |
|---------|-----------|---------|-----------|
| 1 | `core_tables` | 11 | `message_metrics` |
| 2 | `fts_messages` | 12 | `model_dimensions` |
| 3 | `fts_messages_rebuild` | 13 | `plan_token_rollups` |
| 4 | `sources` | 14 | `fts_contentless` |
| 5 | `provenance_columns` | 15 | `conversation_tail_state_cache` |
| 6 | `source_path_index` | 16 | `drop_redundant_message_conv_idx` |
| 7 | `msgpack_columns` | 17 | `drop_message_created_idx` |
| 8 | `daily_stats` | 18 | `conversation_tail_state_hot_table` |
| 9 | `embedding_jobs` | 19 | `conversation_external_lookup` |
| 10 | `token_analytics` | 20 | `conversation_external_tail_lookup` (current) |

**Migration Process**:
1. On startup, `cass` checks `_schema_migrations` in the database (older databases that still record `schema_version` in the `meta` table are transitioned automatically)
2. If version < current, migrations run automatically
3. Migrations are incremental and non-destructive
4. User data (bookmarks, TUI state, sources.toml) is always preserved

**Safe Files** (never deleted during rebuild):
- `bookmarks.db` - Your saved bookmarks
- `tui_state.json` - UI preferences
- `sources.toml` - Remote source configuration
- `.env` - Environment configuration

**Backup and Retention Policy**: Migration/rebuild backups preserve user data and
are not treated as disposable source evidence. Derived lexical publish backups
use the bounded retention policy documented above, while quarantined artifacts
and repair candidates persist until an operator runs an explicit, fingerprinted
cleanup flow.

---

## ⏱️ Watch Mode Internals

The `--watch` flag enables real-time index updates as agent files change.

### Debouncing Strategy

```
File change detected
       ↓
[2 second debounce window]  ← Accumulate more changes
       ↓
[5 second max wait]         ← Force flush if changes keep coming
       ↓
Re-index affected files
```

- **Debounce**: 2 seconds (wait for burst of changes to settle)
- **Max wait**: 5 seconds (don't wait forever during continuous activity)

### Path Classification

Each file system event is routed to the appropriate connector:

```
~/.claude/projects/foo.jsonl  → ClaudeCodeConnector
~/.codex/sessions/rollout-*.jsonl → CodexConnector
~/.aider.chat.history.md → AiderConnector
```

### State Tracking

Watch mode maintains `watch_state.json`:

```json
{
  "last_scan_ts": 1699900000000,
  "watched_paths": [
    "~/.claude/projects",
    "~/.codex/sessions"
  ]
}
```

### Incremental Safety

- **File-level filtering only**: When a file is modified, the entire file is re-scanned
- **1-second mtime slack**: Accounts for filesystem timestamp granularity
- **No per-message filtering**: Prevents data loss when new messages are appended

### Codex Token Backfill

Codex `event_msg` `token_count` usage is attached to the nearest preceding assistant turn during indexing.
If you indexed Codex sessions before this behavior existed, backfill usage coverage with:

```bash
cass index --full
cass analytics rebuild --track a
```

### Rebuilding Analytics Rollups

`cass analytics rebuild` re-derives the Track A rollups (`message_metrics`,
`usage_hourly`, `usage_daily`, `usage_models_daily`) from messages already in
the archive; it never re-parses raw session files. On a large archive a full
rebuild is a long single-core job, so daily refreshes should be windowed:

```bash
# Full rebuild (every rollup row dropped and recomputed)
cass analytics rebuild

# Only recompute the last two UTC days; older rollups are left untouched
cass analytics rebuild --days 2
cass analytics rebuild --since -2d        # same window, relative syntax
cass analytics rebuild --since 2026-08-20 # from a date
```

The window is widened to the start of the UTC day containing the cutoff,
because rollups are bucketed by day and hour. Progress is logged per 10k
messages (`analytics_rebuild_progress`). Across analytics commands, `--days`
and `--since` are mutually exclusive, and malformed or reversed time bounds
return a usage error instead of silently running an unfiltered query.
`--until`, `--agent`, `--workspace`
and `--source` are query-time filters and are rejected here rather than
silently ignored. `--track b` also rejects `--since`/`--days`; with `--track
all`, the window applies to Track A while Track B still rebuilds the complete
`token_usage` ledger. `cass analytics validate` likewise rejects every query
filter because its invariant checks always cover the complete analytics
database.

The TUI analytics dashboard never rebuilds rollups in-process: when rollups are
missing it spawns a detached `cass analytics rebuild` child, logs it to
`<data_dir>/analytics-rebuild.log`, and reports the pid in the status line;
reopen the dashboard once the rebuild finishes.

---

## 🐚 Shell Completions

Generate tab-completion scripts for your shell.

### Installation

**Bash**:
```bash
cass completions bash > ~/.local/share/bash-completion/completions/cass
# Or: cass completions bash >> ~/.bashrc
```

**Zsh**:
```bash
cass completions zsh > "${fpath[1]}/_cass"
# Or add to ~/.zshrc: eval "$(cass completions zsh)"
```

**Fish**:
```bash
cass completions fish > ~/.config/fish/completions/cass.fish
```

**PowerShell**:
```powershell
cass completions powershell >> $PROFILE
```

### What's Completed

- Subcommands (`search`, `index`, `stats`, etc.)
- Flags and options (`--robot`, `--agent`, `--limit`)
- File paths for relevant arguments

---

## System Requirements

- **CPU**: any x86_64 or ARM64 processor. Semantic search runs on a pure-Rust inference backend (frankensearch/native) with runtime-dispatched SIMD — NEON on Apple Silicon, AVX2/FMA when present on x86, SSE2/scalar fallback otherwise — so there is no AVX requirement and no `SIGILL` hazard (the historical ONNX Runtime dependency was removed in cass#308).
- **OS**: Linux, macOS, or Windows
- **Linux glibc**: Pre-built binaries require **glibc 2.38+** (Ubuntu 24.04+, Fedora 39+, Debian 13+). Ubuntu 20.04 (glibc 2.31) and 22.04 (glibc 2.35) are **not supported** with pre-built binaries. Users on older distributions should build from source with `cargo install --git https://github.com/Dicklesworthstone/coding_agent_session_search`. This requirement exists because CI builds target ubuntu-24.04 to access newer kernel features used by the frankensqlite storage engine. The install script does not probe glibc, so on an older distribution pass `--from-source` explicitly (`install.sh --from-source`) or use the `cargo install` route.
- **Disk**: Sufficient space for the search index (varies with session history size)

---

## 🚀 Quickstart

### 1. Install

**Recommended: Homebrew (Apple Silicon macOS + Linux)**
```bash
brew install dicklesworthstone/tap/cass

# Update later
brew upgrade cass
```

The Homebrew tap installs prebuilt release tarballs (not bottles) for Linux and Apple Silicon macOS. On Intel macOS, use the install script with `--from-source`.

**Windows: Scoop**
```powershell
scoop bucket add dicklesworthstone https://github.com/Dicklesworthstone/scoop-bucket
scoop install dicklesworthstone/cass
```

**Alternative: Install Script**
```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/coding_agent_session_search/main/install.sh?$(date +%s)" \
  | bash -s -- --easy-mode --verify
```

**Alternative: GitHub Release Binaries**
1. Download the asset for your platform from GitHub Releases.
2. Verify `SHA256SUMS.txt` against the downloaded archive.
3. Extract and move `cass` into your PATH.

Example (Linux x86_64, replace `VERSION` with an explicit release tag):
```bash
VERSION=v0.2.0  # e.g. v0.2.0
curl -L -o cass-linux-amd64.tar.gz \
  "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/download/${VERSION}/cass-linux-amd64.tar.gz"
curl -L -o SHA256SUMS.txt \
  "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/download/${VERSION}/SHA256SUMS.txt"
sha256sum -c SHA256SUMS.txt
tar -xzf cass-linux-amd64.tar.gz
install -m 755 cass ~/.local/bin/cass
```

### 2. Launch
```bash
cass
```
*On first run, `cass` performs a full index. You'll see progress in the footer. Search works immediately (falling back to SQLite or partial results until complete).*

### 3. Usage
- **Type to search**: "python error", "refactor auth", "c++".
- **Wildcards**: Use `foo*` (prefix), `*foo` (suffix), or `*foo*` (contains) for flexible matching.
- **Navigation**: `Up`/`Down` to select, `Tab` (or `Alt+l`) to focus the detail pane. `Ctrl+N`/`Ctrl+Shift+N` step through query history; `Ctrl+R` cycles it.
- **Filters**:
    - `F3`: Filter by Agent (e.g., "codex").
    - `F4`: Filter by Workspace/Project.
    - `F5`/`F6`: Time filters (Today, Week, etc.).
- **Modes**:
    - `F2`: Next theme (`Shift+F2` previous; 19 presets).
    - `F12`: Cycle ranking mode (recent → balanced → relevance → quality → newest → oldest).
    - `Ctrl+B`: Toggle rounded/plain borders.
- **Actions**:
    - `Enter`: Open selected result in contextual detail modal (defaults to Messages tab).
    - `Enter` with no selected hit: submit query behavior (no-op if empty).
    - `F8`: Open selected hit in `$EDITOR`.
    - `Ctrl+Enter`: Add current result to queue (multi-open).
    - `Ctrl+O`: Open all queued results in editor.
    - `Ctrl+X`: Toggle selection on current item (`Ctrl+M` opens the detail modal, like `Enter`).
    - `Alt+B`: Bulk actions menu (when items selected).
    - `Ctrl+Y` / `Alt+Y` / `Ctrl+Shift+C`: Copy file path / snippet / content to clipboard.
    - `/`: Find text within detail pane; `Enter` advances matches; `n`/`N` cycle contextual session hits; `Esc` closes the modal.
    - `Ctrl+Shift+R`: Trigger manual re-index (refresh search results).
    - `Ctrl+Shift+Del`: Reset TUI state (clear history, filters, layout).

### 4. Multi-Machine Search (Optional)

Aggregate sessions from your other machines into a unified index:

```bash
# Add a remote machine
cass sources add user@laptop.local --preset macos-defaults

# Sync sessions from all sources
cass sources sync

# Check source health and connectivity
cass sources doctor
```

See [Remote Sources (Multi-Machine Search)](#-remote-sources-multi-machine-search) for full documentation.

---

## 🛠️ CLI Reference

The `cass` binary supports both interactive use and automation.

```bash
# Interactive
cass [tui] [--data-dir DIR] [--once] [--asciicast FILE]

# Indexing
cass index [--full] [--watch] [--background] [--data-dir DIR] [--idempotency-key KEY]
cass schedule install [--interval-mins 15] [--nightly-hour 3] [--no-semantic] [--dry-run]
cass schedule status --json

# Search
cass search "query" --robot --limit 5 [--timeout 5000] [--explain] [--dry-run]
cass search "error" --robot --aggregate agent,workspace --fields minimal
cass pack "query" --robot --max-tokens 12000 [--limit 40] [--sessions-from FILE|-]
cass pack "query" --robot --freshness-policy strict --freshness-window-seconds 604800 --require-evidence
cass pack "query" --robot --max-tokens 4000 --max-evidence 8 --max-sessions 3 --max-excerpt-chars 600

# Inspection & Health
cass triage --json                    # One-shot agent preflight with exact next command
cass status --json                    # Quick health snapshot
cass health                           # Minimal pre-flight check (<50ms)
cass capabilities --json              # First-stop agent self-description
cass introspect --json                # Full API schema
cass swarm status --json              # Read-only Beads/Agent Mail/git/rch swarm snapshot
cass swarm work-packet --json         # Advisory claim packet; no mutations
cass swarm lint --json                # Coordination and proof-gap lint
cass context /path/to/session --json  # Find related sessions
cass view /path/to/file -n 42 --json  # View source at line

# Session Analysis
cass export /path/to/session --format markdown -o out.md  # Export conversation
cass expand /path/to/session -n 42 -C 5 --json            # Context around line
cass timeline --today --json                               # Activity timeline

# Remote Sources
cass sources add user@host --preset macos-defaults  # Add machine
cass sources sync                                    # Sync sessions
cass sources doctor                                  # Check connectivity
cass sources mappings list laptop                    # View path mappings

# Utilities
cass stats --json
cass completions bash > ~/.bash_completion.d/cass
```

### Core Commands

| Command | Purpose |
|---------|---------|
| `cass` (default) | Start TUI (a stale index triggers a detached low-priority catch-up; see *Keeping the Index Fresh*) |
| `cass tui --asciicast FILE` | Run TUI and save terminal output as asciicast v2 |
| `index --full` | Discover sessions and refresh the canonical DB plus derived search assets |
| `index --background` | Same as `index`, but lowers its own CPU/I/O priority first (used by auto-refresh, `schedule`, and the daemon timer) |
| `index --watch` | Foreground watch loop: reindex automatically on file changes |
| `schedule install\|uninstall\|status\|run` | Register incremental (15 min) + nightly (full index + semantic backfill) jobs with launchd / systemd user timers |
| `search --robot` | JSON output for automation pipelines |
| `pack --robot` | Deterministic cited answer packs for agent/human handoffs; reports health, freshness, privacy, and warnings |
| `triage` / `ready` / `preflight` | One-shot agent preflight: readiness, exact next command, docs, schemas, workflows, and recoveries |
| `status` / `state` | Health snapshot: index freshness, DB stats, recommended action |
| `health` | Minimal health check (<50ms on a healthy archive; the strict, mutation-free owner-thread probe shared with `status` has a 30 s hard deadline and never checkpoints a dirty WAL), exit 0=healthy, 1=unhealthy |
| `selftest` | Archive-independent executable probe for installers and binary-promotion gates; exercises an in-memory FrankenSQLite write/read round-trip |
| `capabilities` | First-stop agent self-description: workflow recipes, mistake recoveries, commands, global flags, exit codes, env vars, and limits |
| `introspect` | Full API schema: commands, arguments, response shapes |
| `swarm status --json` | Read-only shared-repo operations snapshot across Beads, Agent Mail metadata, git, build pressure, cass readiness, and proof refs |
| `swarm work-packet --json` | Advisory one-agent packet with readiness, suggested reservations, verification commands, and closeout checklist; it does not claim or reserve |
| `swarm lint --json` | Read-only coordination protocol lint for missing mail, stale reservations, status mismatches, and proof gaps |
| `swarm dependency-drift --json` | Read-only sibling dependency sentinel for Cargo.toml pins, optional local checkout HEAD/dirty state, strict validation commands, and release-risk recommendations |
| `sessions [--workspace DIR] [--current]` | Discover recent session files for follow-up actions |
| `context <path>` | Find related sessions by workspace, day, or agent |
| `view <path> -n N` | View source file at specific line (follow-up on search) |
| `export <path>` | Export conversation to markdown/JSON |
| `export-html <path>` | Export as self-contained HTML with optional encryption |
| `expand <path> -n N` | Show messages around a specific line number |
| `timeline` | Activity timeline with grouping by hour/day |
| `sources` | Manage remote sources: add/list/remove/doctor/sync/mappings |
| `doctor` | Diagnose and repair installation issues (safe, never deletes data) |

Other subcommands (all present in the `Commands` enum in `src/lib.rs`):

| Command | Purpose |
|---------|---------|
| `pages` | Export an encrypted, searchable static-site archive with GitHub Pages / Cloudflare Pages deploy; runs the interactive wizard by default, with `--export-only DIR`, `--verify BUNDLE`, `--preview BUNDLE`, and `--scan-secrets` as non-wizard modes |
| `upgrade` | Check for a newer release and optionally run the same checksum-verified installer the TUI uses (`--check`, `--yes`, `--force`) |
| `man` | Generate the man page to stdout |
| `storage` | On-disk storage footprint by component (DB, WAL, lexical index, raw mirror, semantic, quarantine) |
| `dedup` | Collapse pre-existing duplicate conversation rows (`projects/<rel>` vs `<rel>` external-id twins); dry-run unless `--apply` |
| `support-bundle` | Assemble a redacted, share-safe recovery/support evidence bundle |
| `state` | Quick state/health check (alias of `status`) |
| `onboarding` | Read-only first-run source onboarding + readiness wizard; `--json` for scripts, never launches the TUI |
| `quarantine` | Inspect and manage the conversation-ingest quarantine (`list` / `clear`) |
| `forget` | Prune already-indexed conversations by source-path glob; dry-run by default, `--apply` to commit, then derived search/analytics assets are rebuilt |
| `fleet upgrade-rehearsal` | Fleet-safe upgrade rehearsal (dry run) with bounded post-upgrade verification; `--live` opts in to SSH probes of configured remotes |
| `lessons list\|search` | Mine and query durable, redacted lessons from local evidence (commits, closed beads, proof manifests) |
| `import chatgpt` | Split a ChatGPT web export (`conversations.json`) into files the ChatGPT connector can index |
| `release-verify` | Verify release distribution channels (GitHub, Homebrew, Scoop, crates.io, installer) from a recorded observation (`--from`) or live (`--live`) |
| `sources discover` | Auto-discover SSH hosts from `~/.ssh/config` |
| `sources reingest` | Re-ingest an already-synced mirror into the canonical archive without re-running rsync |
| `sources artifact-manifest` | Build or verify a lexical-artifact evidence manifest for remote exchange |

### Specialized Validation and Recording Tools

| Tool | Purpose |
|------|---------|
| `cass tui --asciicast FILE` | Record TUI output as an asciicast v2 artifact; there is no separate `cass cast` subcommand |
| `scripts/bakeoff/cass_validation_e2e.sh` | Run the bake-off validation harness for lexical, semantic, hybrid, and reranked search scenarios |
| `scripts/bakeoff/cass_embedder_e2e.sh` | Exercise embedder bake-off flows against a generated validation corpus |
| `scripts/bakeoff/cass_rerank_e2e.sh` | Exercise reranker bake-off flows and append results to the bake-off log |

### Diagnostic Commands

Commands for troubleshooting, debugging, and understanding system state:

```bash
# One-shot agent preflight
cass triage --json
# → { "surface": "triage", "status": "healthy", "next_command": null, ... }

# Health check (fast, <50ms; the archive probe is bounded by a 30 s hard deadline)
cass health --json
# → { "healthy": true, "index_age_seconds": 120, "message_count": 5000 }

# Detailed status with recommendations
cass status --json
# → Includes index freshness, staleness threshold, recommended action

# System diagnostics
cass diag --verbose --json
# → Database stats, index info, connector status, environment

# Query explanation (debug why results are what they are)
cass search "auth" --explain --dry-run --robot
# → Shows parsed query, index strategy, cost estimate without executing

# Find related sessions
cass context /path/to/session.jsonl --json
# → Sessions from same workspace, same day, or same agent

# Archive-first diagnostic check
cass doctor check --json
# → Read-only checks for archive coverage, source authority, locks, backups,
#   storage pressure, semantic fallback, and recommended next action

# Fingerprinted repair plan and apply
cass doctor repair --dry-run --json
cass doctor repair --yes --plan-fingerprint <plan_fingerprint> --json
# → Builds candidates and applies only the inspected matching fingerprint

# Legacy safe auto-run for low-risk derived repairs
cass doctor --fix --json
# → Emits operation_outcome and receipts; fails closed on archive/source risk
```

### The Doctor Command

`cass doctor` is a comprehensive diagnostic and repair tool designed for troubleshooting installation and data issues. Its current recovery model is **archive-first**: preserve cass-owned evidence, prove source authority and coverage, then repair through candidates and receipts. The full operator runbook is [`docs/planning/RECOVERY_RUNBOOK.md`](docs/planning/RECOVERY_RUNBOOK.md).

**What it checks:**

| Surface | Purpose | Mutation Policy |
|---------|---------|-----------------|
| `cass doctor check --json` | Read-only truth surface for archive coverage, source authority, locks, storage pressure, semantic fallback, and recommended action | Never mutates |
| `cass doctor archive-scan --json` | Read-only source inventory, raw mirror, coverage, sole-copy, and remote sync gap inspection | Never mutates |
| `cass doctor repair --dry-run --json` | Builds a fingerprinted repair plan and candidate/promotion gates | Read-only plan |
| `cass doctor repair --yes --plan-fingerprint <fp> --json` | Applies exactly the inspected repair fingerprint | Candidate-based, receipt-backed |
| `cass doctor backups list/verify/restore ... --json` | Lists backups, verifies manifests, rehearses restore, then applies by fingerprint | Restore apply requires a matching rehearsal fingerprint |
| `cass doctor cleanup --json` | Plans cleanup for derived or explicitly reclaimable assets | Apply requires a matching fingerprint |
| `cass doctor support-bundle --json` | Creates a scrubbed diagnostic handoff bundle | Redacted by default; not a backup |

**Safety guarantees:**

- **Preserves source evidence** - Claude, Codex, Cursor, Gemini, remote mirrors, raw-mirror blobs, manifests, and source ledgers are treated as evidence.
- **Preserves archive state** - canonical SQLite archives, WAL/SHM sidecars, backup bundles, restore receipts, bookmarks, TUI state, and `sources.toml` are not cleanup targets.
- **Separates diagnosis from mutation** - check, archive-scan, baseline diff, backup verify, and support-bundle verify are read-only.
- **Requires fingerprints for risky mutations** - repair, restore apply, cleanup apply, archive normalize apply, and archive export apply consume the exact dry-run `plan_fingerprint`.
- **Fails closed on coverage risk** - source pruning, sole-copy warnings, ambiguous authority, failed probes, or repeated repair markers block unsafe repair until inspected.
- **Keeps support bundles scrubbed** - default bundles include redacted summaries and checksummed manifests, not raw session logs or full archive copies.

**Recommended support checklist:**

```bash
cass doctor check --json
cass doctor baseline diff <baseline_id> --json
cass doctor support-bundle --json
cass doctor support-bundle verify <bundle_or_manifest_path> --json
```

Send the doctor JSON, latest `failure_context.json` if present, support-bundle
`manifest.json`, any baseline diff, relevant `artifact_manifest_path` and
`event_log_path` values, and the exact command/exit code. Do not attach raw
sessions, full SQLite archives, private source files, or encrypted payloads
unless the user explicitly opts into sensitive evidence attachment.

**Diagnostic Flags**:
| Flag | Available On | Effect |
|------|--------------|--------|
| `--explain` | search | Show query parsing and strategy |
| `--dry-run` | search | Validate without executing |
| `--verbose` | most commands | Extra detail in output |
| `--trace-file` | all | Append execution trace to file |
| `--robot-trace-ingest` | index | Emit per-ingest-batch NDJSON timing and lookup counters on stderr |

### Model Management

Commands for managing the semantic search ML model:

```bash
# Check current model status (abbreviated schema — real output also
# includes cache_lifecycle, files[], revision, license, and more):
cass models status --json
# → {
#     "model_id": "all-minilm-l6-v2",
#     "model_dir": "~/.local/share/coding-agent-search/models/all-MiniLM-L6-v2",
#     "installed": false,
#     "state": "not_acquired",
#     "state_detail": "model not acquired (user consent required); missing ...",
#     "next_step": "Run `cass models install`, or keep using lexical search.",
#     "lexical_fail_open": true,
#     "revision": "c9745ed1...",
#     "license": "Apache-2.0",
#     "total_size_bytes": 90872535,
#     "installed_size_bytes": 0,
#     "observed_file_bytes": 0,
#     "policy_source": "semantic_policy"
#   }

# Install model (downloads ~90MB from Hugging Face on explicit request)
cass models install
# → Downloads from Hugging Face, verifies checksum

# Install from local directory (air-gapped environments)
cass models install --from-file /path/to/model-dir

# Verify model integrity
cass models verify --json
# → all_valid bool + per-file SHA-256 checks (see `cass models verify --help`)

# Check for model updates
cass models check-update --json
# → { "update_available": bool, "reason": str,
#     "current_revision": str|null, "latest_revision": str }
```

In `cass status --json`, `semantic.preferred_backend` is `"fastembed"` when the native MiniLM lane is selected and `"hash"` for the hash tier; `fastembed` is only the id of the native pure-Rust MiniLM lane — no ONNX runtime is involved.

**Model Files** (stored in `$CASS_DATA_DIR/models/all-MiniLM-L6-v2/`):
- `model.safetensors` - The neural network weights (~90MB)
- `tokenizer.json` - Vocabulary and tokenization rules
- `config.json` - Model configuration
- `special_tokens_map.json` - Special token definitions
- `tokenizer_config.json` - Tokenizer settings

---

## 🔒 Integrity & Safety

- **Verified Install**: The installer enforces SHA256 checksums.

- **Sandboxed Data**: All indexes/DBs live in standard platform data directories (`~/.local/share/coding-agent-search` on Linux).

- **Read-Only Source**: `cass` *never* modifies your agent log files. It only reads them.

### Atomic File Operations

`cass` uses crash-safe atomic write patterns throughout to prevent data corruption:

**TUI State Persistence** (`tui_state.json`):
```
1. Serialize state to JSON
2. Write to temporary file (tui_state.json.tmp)
3. Atomic rename: temp → final
```
If a crash occurs during step 2, the original file is untouched. The rename operation (step 3) is atomic on all modern filesystems—it either completes fully or not at all.

**ML Model Installation** (`models/all-MiniLM-L6-v2/`):
```
1. Download to temp directory (models/all-MiniLM-L6-v2.tmp/)
2. Verify all checksums
3. If existing model present: rename to backup (models/all-MiniLM-L6-v2.bak/)
4. Atomic rename: temp → final
5. On success: remove backup
6. On failure: restore from backup
```
This backup-rename-cleanup pattern ensures that either the old model or new model is always available—never a half-installed state.

**Configuration Files** (`sources.toml`, `watch_state.json`):
All configuration writes follow the same temp-file-then-rename pattern, ensuring consistency even during power loss or unexpected termination.

**Why This Matters**:
- System crashes mid-write won't corrupt your preferences
- Network interruptions during model download won't leave broken installations
- Concurrent processes won't see partially-written files



## 📦 Installer Strategy

The project ships with a robust installer (`install.sh` / `install.ps1`) designed for CI/CD and local use:

- **Checksum Verification**: Validates artifacts against a `.sha256` file or explicit `--checksum` flag.

- **Rustup Bootstrap**: Source installs use the dated nightly and components pinned by the release's `rust-toolchain.toml`. The installer bootstraps rustup without an unrelated default toolchain when needed.

- **Easy Mode**: `--easy-mode` automates installation to `~/.local/bin` without prompts.

- **Platform Agnostic**: Detects OS/Arch (Linux/macOS/Windows, x86_64/arm64) and fetches the correct binary.



## 🔄 Automatic Update Checking

`cass` includes a built-in update checker that notifies you when new versions are available, without interrupting your workflow.

### How It Works

1. **Background Check**: On TUI startup, a background thread queries GitHub releases
2. **Rate Limiting**: Checks run at most once per hour to avoid API rate limits
3. **Non-Blocking**: Update checks never slow down TUI startup or search operations
4. **Offline-Safe**: Failed network requests are silently ignored

### Update Notifications

When a new version is available, a notification appears in the TUI with:
- Current version vs. available version
- Release highlights (from GitHub release notes)
- Options: **Update Now** | **Skip This Version** | **Remind Later**

### Self-Update Installation

Selecting "Update Now" runs the same verified installer used for initial installation:

macOS/Linux:

```bash
curl -fsSL https://...install.sh | bash -s -- --easy-mode --verify
```

Windows (PowerShell):

```powershell
& ([scriptblock]::Create((irm "https://...install.ps1"))) -EasyMode -Verify
```

The update process:
1. Downloads `install.sh` (or `install.ps1`) and verifies it against the release `SHA256SUMS.txt`
2. Replaces the current process with `install.sh --easy-mode --verify --version <tag>` (PowerShell equivalent on Windows), which downloads and checksum-verifies the new binary
3. There is no separate binary backup or rollback step; reinstall an earlier tag with the installer's `--version` flag if needed

### Skip Version

If you're not ready to update, "Skip This Version" records the skipped version in persistent state. That specific version won't trigger notifications again, but future versions will.

### Disable Update Checks

For automated environments or personal preference:

```bash
# Environment variable
export CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1

# Or for fully headless operation
export TUI_HEADLESS=1
```

### State Persistence

Update check state is stored in the data directory:
- `last_update_check`: Timestamp of last check (for rate limiting)
- `skipped_version`: Version user chose to skip
- Both are reset on manual update or by deleting `tui_state.json`

---

## ⚙️ Environment

- **Config**: Loads `.env` via `dotenvy::dotenv().ok()`; configure API/base paths there. Do not overwrite `.env`.

- **Data Location**: Defaults to standard platform data directories (e.g., `~/.local/share/coding-agent-search`). Override with `CASS_DATA_DIR` or `--data-dir`.

- **ChatGPT Support**: The ChatGPT macOS app stores conversations in versioned formats:
  - **v1** (legacy): Unencrypted JSON in `conversations-{uuid}/` — fully indexed.
  - **v2/v3**: Encrypted with AES-256-GCM, key stored in macOS Keychain (OpenAI-signed apps only) — detected but skipped.

  Encrypted conversations require keychain access which isn't available to third-party apps. Legacy unencrypted conversations are indexed automatically.

- **Logs**: Written to `cass.log` (daily rotating) in the data directory.

- **Updates**: Interactive TUI checks for GitHub releases on startup. Skip with `CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1` or `TUI_HEADLESS=1`.

- **Cache tuning**: `CASS_CACHE_SHARD_CAP` (per-shard entries, default 256) and `CASS_CACHE_TOTAL_CAP` (total cached hits across shards, default 2048) control prefix cache size; raise cautiously to avoid memory bloat.

- **Cache debug**: set `CASS_DEBUG_CACHE_METRICS=1` to emit cache hit/miss/shortfall/reload stats via tracing (debug level).

- **Temporary scan exclusions**: `CASS_EXCLUDE_PATHS` accepts comma- or newline-delimited file paths or directory prefixes to skip during source discovery and parsing. While exclusions are active, CASS preserves scan/watch watermarks so excluded active session files are picked up after the exclusion is removed.

- **Watch testing (dev only)**: `cass index --watch --watch-once path1,path2` triggers a single reindex without filesystem notify (also respects `CASS_TEST_WATCH_PATHS` for backward compatibility); useful for deterministic tests/smoke runs.

### Complete Environment Variable Reference

| Variable | Default | Description |
|----------|---------|-------------|
| **Core** | | |
| `CASS_DATA_DIR` | Platform default | Override data directory |
| `CASS_DB_PATH` | `$CASS_DATA_DIR/agent_search.db` | Override database path |
| `CASS_EXCLUDE_PATHS` | unset | Comma/newline-delimited files or directory prefixes to skip without advancing scan/watch watermarks |
| `CASS_DOCTOR_RAW_MIRROR_FULL_VERIFY` | unset | Set to `1` to hash every raw-mirror descriptor/chunk during a read-only doctor run, overriding the default bounded verification limits |
| `CASS_DOCTOR_RAW_MIRROR_FULL_VERIFY_MANIFEST_LIMIT` | `256` | Defer full raw-mirror hashing above this manifest count while retaining metadata-only amplification diagnostics |
| `CASS_DOCTOR_RAW_MIRROR_FULL_VERIFY_BYTE_LIMIT` | `536870912` | Defer full raw-mirror hashing when either physical storage or estimated logical verification work exceeds this byte count; metadata-only amplification diagnostics remain available |
| `CASS_FTS_DRYRUN_CAP` | `4096` | Row-ID comparison cap for the read-only `doctor --rebuild-canonical-fts --dry-run` divergence scan ([#345](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/345)). At the cap the dry-run stops and reports a `>= N divergent` floor instead of an exact count; exact parity is deferred to the `--yes` apply path |
| **Background Indexing** | | |
| `CASS_AUTO_REFRESH` | `1` | Stale-on-read catch-up for a stale/partial/behind index seen by `search`, `pack`, or TUI launch. Set `0` to disable globally; `search --no-maintenance` always remains read-only. |
| `CASS_AUTO_REFRESH_COOLDOWN_SECS` | `300` | Minimum spacing between auto-spawned catch-up runs per data dir |
| `CASS_BACKGROUND_NICE` | `15` | nice value `cass index --background` applies to itself (0..=19) |
| `CASS_BACKGROUND_IONICE_CLASS` | `3` | ionice class for `cass index --background` on Linux (3 = idle) |
| `CASS_DAEMON_INDEX_INTERVAL_SECS` | `0` | While the semantic daemon is resident, spawn an incremental background index every N seconds (`900` recommended; 0 = off) |
| `CASS_SCHEDULE_MAX_BACKFILL_BATCHES` | `200` | Cap on `models backfill --scheduled` batches per nightly `cass schedule run` |
| `CASS_RESPONSIVENESS_MIN_USER_IDLE_SECS` | `0` | Require N seconds of console idle (macOS `HIDIdleTime`) before the nightly job / scheduled backfill runs; fails open where unavailable |
| **Indexing & Redaction** | | |
| `CASS_INDEX_REDACTION` | `full` | Index-time secret redaction: `full` scrubs API keys/tokens/passwords/private keys from every persisted message, title, snippet, and metadata blob before they reach SQLite or the lexical index; `off` skips redaction for faster ingest. **`off` means raw text is indexed** — note that the original session files and the cass raw-mirror blobs (`<data_dir>/raw-mirror/v1/`) already contain the same raw text unencrypted on the same disk, so `full` protects the queryable surfaces (search results, exports, robot output), not disk-at-rest secrecy. Unrecognized values warn and behave as `full`. |
| `CASS_REDACT_SECRETS` | `1` | Legacy redaction toggle (`0`/`false`/`off`/`no` disables). `CASS_INDEX_REDACTION` takes precedence when both are set. |
| `CASS_REDACT_MEMO_CAPACITY` | 4096 | Entry cap for the per-worker redaction memo cache used during batched persist. Raise on very large, boilerplate-heavy corpora if eviction churn shows up in `cass::redact::memo` debug logs. Only strings the secret prefilter flags as candidate-bearing are memoized, and individual inputs over 64 KiB are never cached (bounds worst-case cache memory). |
| `CASS_INDEX_STALL_DETECT_SECS` | `120` | Seconds without measured phase progress before `cass index` emits stall diagnostics; `0` disables detection |
| `CASS_INDEX_STALL_ABORT_SECS` | `300` | Seconds without progress before an abort-eligible lexical stall exits 70; `0` keeps stalls report-only. Semantic phases are report-only unless `CASS_INDEX_STALL_ABORT_ALL_PHASES=1` |
| `CASS_INDEX_STALL_ABORT_ALL_PHASES` | unset | Opt-in ([#437](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/437)): set `1` to promote the stall watchdog's report-only warnings to a hard abort (exit 70) in ANY phase — including scribe/accumulate and semantic report-only lanes — once `CASS_INDEX_STALL_ABORT_SECS` elapses without progress. The finalize/persist grace thresholds still apply; the abort finalizes exactly like lexical-phase aborts (best-effort WAL checkpoint, lock release via startup recovery) |
| `CASS_INDEX_FINALIZE_ABORT_SECS` | `1800` | Larger abort grace applied while the indexer is inside the finalize WAL-checkpoint / rebuild-tail windows; `0` makes the finalize window report-only |
| `CASS_NO_COLOR` | unset | Force monochrome TUI output |
| `NO_COLOR` | unset | Honored by TUI only when `CASS_RESPECT_NO_COLOR=1` |
| `CASS_RESPECT_NO_COLOR` | unset | Make TUI inherit global `NO_COLOR` |
| **Search & Cache** | | |
| `CASS_CACHE_SHARD_CAP` | 256 | Per-shard LRU cache entries |
| `CASS_CACHE_TOTAL_CAP` | 2048 | Total cached search hits |
| `CASS_CACHE_BYTE_CAP` | 10485760 | Cache byte limit (10MB) |
| `CASS_WARM_DEBOUNCE_MS` | 120 | Warm-up search debounce |
| `CASS_DEBUG_CACHE_METRICS` | unset | Enable cache hit/miss logging |
| `CASS_QUILL_QUERY_FUEL_BUDGET` | Quill default (10000000) | Escape hatch for Quill's deterministic per-query work ceiling (GH #441). Zero or unparseable values keep the engine default. When fuel runs out on a hybrid query the lexical leg is dropped, the semantic leg still answers, and `_meta.lexical_degrade_reason` reports `query_fuel_exhausted`; lexical-only queries return an actionable hint. The durable fix for fuel exhaustion is a consolidated index (`cass index --full`), and cass now publishes Quill snapshots only on its own commits (no per-second visibility seals), which is what let segment counts grow into the hundreds on append-only archives |
| **Semantic Search** | | |
| `CASS_SEMANTIC_EMBEDDER` | auto | Force embedder: `hash`, `minilm`, or explicit `multilingual-minilm` |
| `CASS_SEMANTIC_PROGRESS_JSONL` | unset | Absolute path to a JSONL file the semantic backfill appends one event per transition to (`selection_*`, `packet_replay_*`, `embed_batch_*`, `staging_write_*`, `checkpoint_save_*`, `publish_*`, `error`, `cancelled`, `complete`). Each line carries timestamp, phase + sub-phase, batch/row counters, byte counts, elapsed-since-start, and a cheap RSS estimate. Silent when unset. Best-effort writes — failures log at debug and never crash a backfill. See [cass#257](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/257). |
| `CASS_SEMANTIC_EMBED_BATCH_WARN_AFTER_MS` | 30000 | Warn when one embedder batch takes more than 30s. Derived from cass#257 boxed quality-corpus telemetry: 60 MiniLM batches averaged ~5.95s, so the default is about 5x the observed healthy batch. Set `0` to disable warnings. |
| `CASS_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS` | 300000 | Abort a semantic backfill batch after a single embedder batch returns if it exceeded 5 minutes. Derived as a conservative ~50x multiple of the cass#257 healthy 128-doc MiniLM batch average. Set `0` to disable failure. |
| `CASS_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT` | 10000 | Soft cap for `cass models backfill`: checkpoint after a whole-conversation prefix near 10k selected messages. Derived from cass#257 high-volume proof (7,618 docs in ~6 minutes) plus the original 10k-message workaround. Set `0` for no message cap. |
| `CASS_SEMANTIC_MAX_BYTES_PER_CHECKPOINT` | 8388608 | Soft cap for `cass models backfill`: checkpoint after a whole-conversation prefix near 8 MiB selected content. Derived from cass#257 high-volume proof (4.3 MiB selected bytes) with about 2x headroom. Set `0` for no byte cap. |
| **TUI** | | |
| `TUI_HEADLESS` | unset | Disable interactive features |
| `CASS_ALLOW_DUMB_TERM` | unset | Allow TUI startup even when `TERM=dumb` |
| `CASS_UI_METRICS` | unset | Enable UI interaction tracing |
| `CASS_DISABLE_ANIMATIONS` | unset | Disable UI animations |
| `EDITOR` | `$VISUAL` or `vi` | External editor command |
| `EDITOR_LINE_FLAG` | `+` | Line number flag (e.g., `+42`) |
| **Updates** | | |
| `CODING_AGENT_SEARCH_NO_UPDATE_PROMPT` | unset | Disable update notifications |
| **Connector Overrides** | | |
| `CASS_AIDER_DATA_ROOT` | `~/.aider.chat.history.md` | Aider history location |
| `CASS_OMP_DATA_ROOT` | unset | OMP-only archive/store root; assigns OMP identity and preserves the exact sessions root for resume |
| `PI_SESSIONS_DIR` | unset | Exact Pi-Agent sessions directory |
| `PI_CODING_AGENT_DIR` | unset | Shared pi-family agent home; ambiguous paths remain Pi-Agent-owned in CASS (use `CASS_OMP_DATA_ROOT` for OMP-only ownership) |
| `PI_CODING_AGENT_SESSION_DIR` | unset | Exact OMP sessions directory |
| `OMP_PROFILE` | unset | Active OMP profile; takes precedence over `PI_PROFILE` |
| `PI_PROFILE` | unset | Legacy OMP profile selector when `OMP_PROFILE` is unset |
| `PI_CONFIG_DIR` | `.omp` | Home-relative OMP config directory name |
| `XDG_DATA_HOME` | platform default | OMP XDG data root (`$XDG_DATA_HOME/omp`) when present |
| `CASS_MUSE_DATA_ROOT` | `~/.local/share/muse` | Muse Code data root |
| `CODEX_HOME` | `~/.codex` | Codex data directory |
| `GEMINI_HOME` | `~/.gemini` | Gemini CLI directory |
| `OPENCODE_STORAGE_ROOT` | (scans home) | OpenCode storage |
| `CHATGPT_ENCRYPTION_KEY` | unset | Base64-encoded AES key for ChatGPT v2/v3 |

---

## Dependency Source Contract

`cass` pins its contract-critical ecosystem dependencies with exact registry requirements in [`Cargo.toml`](Cargo.toml); other direct dependencies use normal semver requirements, and `Cargo.lock` freezes the complete resolved graph. No active dependency or patch currently resolves from git. Optional sibling-path overrides stay commented out by default and must never be committed active.

| Dependency | Pinned source |
|------------|-----------------|
| `frankensqlite` / `fsqlite-types` | crates.io `=0.3.13` (includes the asupersync 0.4.3 migration, GH#333/GH#334 fixes including cass#393, allocator/freelist/concurrent-writer correctness work, FTS5 hardening, Windows sidecar-less read-only close repair, checkpoint/recovery and CHECK-enforcement hardening, and cass#434 autoindex-vanish corruption-writer fixes). The whole family resolves from one exact registry version; `build.rs` rejects any fsqlite-family registry patch, duplicate package resolution, wrong version, or non-crates.io lockfile source. The engine API is async; `src/franken_sync.rs` preserves cass's synchronous call shape via a current-thread asupersync `block_on` bridge. |
| `franken-agent-detection` | crates.io `=0.2.1` (2026-08-24; `src/` byte-identical to the previously pinned git rev `82424dc8` — first-class Oh My Pi v18 connector with profile, XDG, direct-root, and sub-agent discovery; preserves pi-family remote provenance; includes Muse Code and VS Code Copilot; aligned with fsqlite 0.3.x + asupersync 0.4.x) |
| `asupersync` | `=0.4.9` (fsqlite 0.3.x requires the 0.4.x line, whose types its public API names; 0.4.9 preserves the typed-result cancellation contract) |
| `frankensearch` | crates.io `=0.4.2` (2026-08-28, cass#410/frankensearch#40: adds the distinct opt-in `paraphrase-multilingual-MiniLM-L12-v2` native embedding space, immutable artifact/producer identity, explicit-only acquisition, and dynamic 6/12-layer Frankentorch loading to the 0.4.1 Windows Quill publication line. It retains the CASS→Quill lexical flip with `cass-compat` → `lexical-tantivy` for the differential oracle and schema-generation sentinel, pure-Rust `native`, architecture-safe HNSW, consumer-owned `TwoTierIndexPaths`, non-mutating lexical admission, cancellation-safe facade opening, and generation-pinned Quill hydration. Registry `0.3.2` is a stale same-version twin of an older tree (no quill/cass-compat/native) — the exact pin exists so resolution can never reach it. frankentorch resolves from crates.io as `frankentorch-*`; the HNSW fork is registry `frankenhnsw 0.3.5`; tantivy is registry `=0.26.1` (RUSTSEC-2026-0253 on its lru is unreachable: the advisory needs a panicking `Drop` on a cache key under `catch_unwind`, and tantivy's cache keys are trivially droppable) — cass #308, #333, #410, bd-8nqz.5, bd-07os, bd-r65a.1) |
| `frankentui` (`ftui`, `ftui-runtime`, `ftui-tty`, `ftui-extras`) | crates.io `=0.5.0` (2026-08-21; previously git `5f78cfa0` / 0.3.1 — the 0.5 API compiled with zero call-site changes) |
| `toon` (`tru`) | crates.io `=0.2.4` (2026-08-24; production sources byte-identical to the previously pinned git rev `d7185c78` — registry 0.2.3 was rejected because its tree differs from the rev in real source despite the matching version field) |

**Build-time validation**
- `build.rs` validates every named dependency contract against its exact registry requirement, package name, enabled features, and `default-features` policy. It also rejects git/revision fields for these registry-only contracts.
- The fsqlite-family gate additionally checks `Cargo.lock` for one converged version resolved from the pinned upstream revision, requires the single facade `[patch.crates-io].fsqlite` redirect, and rejects any other patch entry for that family.
- Enable optional sibling-manifest validation with `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-strict-target cargo check --features strict-path-dep-validation` or `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-strict-target CASS_STRICT_PATH_DEP_VALIDATION=1 cargo check`. For sibling checkouts that are present, this verifies package names, versions, and required features before you switch to local path overrides; registry-only contracts do not require a particular sibling branch or clean worktree.
- Use `cass swarm dependency-drift --json` for a fast read-only preflight. It reports each manifest pin, optional sibling checkout HEAD/dirty state, upstream status as `not_checked`, and the exact strict-validation commands to run; it never fetches remotes or mutates files.

**Expected interface contract**
- `frankensqlite` (`fsqlite`): `Connection`, `params!`, and `compat::{ConnectionExt, RowExt}` with `row.get_typed(...)`.
- `franken-agent-detection`: `AgentDetectOptions` and `detect_installed_agents(...)`.
- `frankensearch`: the Quill CASS document/reader/writer surface used by the active lexical backend, the retained `lexical_tantivy` compatibility surface used by legacy federated assembly and differential checks, plus `ModelCategory` and `ModelTier`.
- `frankentui`: `ftui::Frame`, `GraphemePool`, `Style`, `ftui-runtime`, `ftui-tty`, and the `ftui-extras` features enabled by cass.
- `asupersync`: `runtime::RuntimeBuilder` and `http::h1::HttpClient::builder()`.
- `toon` (`tru`): `toon::encode(...)`.

When intentionally updating one of these sibling crates, update the manifest pin, the `build.rs` contract, and the compile-contract test together.

---

## 🩺 Troubleshooting

- **TUI looks monochrome / “1981 mode”**: Check `TERM` and `NO_COLOR`.
  Full-style launch example:
  ```bash
  env -u NO_COLOR TERM=xterm-256color COLORTERM=truecolor cass
  ```
  If you intentionally want monochrome, use `CASS_NO_COLOR=1 cass`.
  If a wrapper keeps forcing `TERM=dumb` and UI still degrades, either fix `TERM` or force raw mode explicitly with `CASS_ALLOW_DUMB_TERM=1 cass`.

- **Checksum mismatch**: Ensure `.sha256` is reachable or pass `--checksum` explicitly. Check proxies/firewalls.

- **Binary not on PATH**: Append `~/.local/bin` (or your `--dest`) to `PATH`; re-open shell.

- **Rust toolchain missing in CI**: Set `RUSTUP_INIT_SKIP=1` if the pinned toolchain is preinstalled; otherwise allow installer to run rustup.

- **Watch mode not triggering**: Confirm `watch_state.json` updates and that connector roots are accessible; `notify` relies on OS file events (inotify/FSEvents).

- **Reset TUI state**: Run `cass tui --reset-state` (or press `Ctrl+Shift+Del` in the TUI) to delete `tui_state.json` and restore defaults.



## 🧪 Developer Workflow

We target the dated Rust nightly `nightly-2026-08-25` pinned by
`rust-toolchain.toml`. Agents should offload build, test, lint, and snapshot
commands with `rch`.

```bash
# Format & Lint
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dev-target cargo fmt --check
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dev-target cargo clippy --all-targets -- -D warnings

# Build & Test
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dev-target cargo build --release
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dev-target cargo test

# Run End-to-End Tests
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dev-target cargo test --test e2e_index_tui
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dev-target cargo test --test install_scripts
```

### Snapshot Baseline Workflow (FrankenTUI)

Use targeted snapshot runs; do not blindly bless everything:

```bash
# Verify current baselines
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-snapshot-target cargo test snapshot_baseline_ -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-snapshot-target cargo test snapshot_search_surface_ -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-snapshot-target cargo test --test ftui_harness_snapshots -- --nocapture

# Regenerate only the suite you intentionally changed
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-snapshot-target BLESS=1 cargo test snapshot_baseline_ -- --nocapture
```

The full regeneration/review protocol (required reviewer checklist, behavioral guard tests,
and quality gates) lives in `docs/planning/TESTING.md` under
`Snapshot Baseline Regeneration & Review (FrankenTUI)`.

### HTML Export E2E Logging

Playwright E2E runs emit a setup metadata file at `tests/e2e/exports/setup-metadata.json` and
export its path as `TEST_EXPORT_SETUP_LOG` in `tests/e2e/.env.test`. On failures, tests attach
per-test browser logs (console/pageerror/requestfailed). Set `E2E_LOG_ALWAYS=1` to attach logs
for every test. These paths are generated run artifacts and are intentionally ignored; durable
logging schema documentation lives in `docs/reference/E2E_LOGGING_SCHEMA.md`.

### Release Build Optimizations

The release profile is aggressively optimized for binary size and performance:

```toml
[profile.release]
lto = true              # Link-time optimization across all crates
codegen-units = 1       # Single codegen unit for better optimization
strip = true            # Remove debug symbols from binary
panic = "abort"         # Smaller panic handling (no unwinding)
opt-level = 3           # Maximum runtime optimization
```

**Trade-offs**:
- Build time is significantly longer (~3-5x)
- LTO and a single codegen unit prioritize runtime optimization at the cost of build time
- No stack traces on panic (use debug builds for development)

### CI Pipeline & Artifacts

The CI pipeline (`.github/workflows/ci.yml`) is defined to run on every PR and push to main. **Note:** every workflow defined in `.github/workflows/` is currently disabled (`gh workflow list --all` shows `disabled_manually` for all of them); until CI is re-enabled the same gates are run by agents through `rch`:

| Job | Purpose | Artifacts |
|-----|---------|-----------|
| `check` | fmt, clippy, tests, benches, UBS scan | None |
| `e2e` | Integration tests (install, index, filters) | `test-artifacts-e2e` (traces, logs) |
| `coverage` | Code coverage with llvm-cov | `coverage-report` (lcov.info, summary) |

**Coverage Reports:**
- `lcov.info` - LCOV format for tools like codecov
- `coverage-summary.txt` - Human-readable summary
- Coverage % shown in GitHub Actions step summary

**Test Artifacts:**
- Trace files from `--trace-file` runs
- Test run summary logs
- Retained for 7 days (e2e) / 30 days (coverage)

```bash
# Generate coverage through rch
# Ensure cargo-llvm-cov is already installed before agent-run gates.
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --all-features --workspace --text

# Run specific e2e tests
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-e2e-target cargo test --test e2e_filters -- --test-threads=1
```

## About Contributions

> *About Contributions:* Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---
