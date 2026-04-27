# PLAN_FOR_CODING_AGENT_SEARCH

**Progress 2025-11-21:** Schema/migration v3 (fts5 mirror with created_at) + rusqlite DAL; connectors implemented (Codex, Cline, Gemini, Claude; Amp/OpenCode detect-only) with Codex fixture test; index command persists to SQLite and Tantivy (agent/workspace/source_path/msg_idx/created_at/title/content) with optional watch scaffold; CLI/TUI shell on nightly; Search client supports Tantivy + SQLite-FTS fallback, agent/workspace/time filters, pagination; TUI renders live results + detail pane and status guidance.

Ultra-high-level:
Build a single Rust binary (`agent-search`, name TBD) that:

* Runs a **slick, low-latency TUI** (ratatui + crossterm) on Linux/macOS/Windows
* Auto-detects Codex CLI, Claude Code, Gemini CLI, Amp CLI, Cline, OpenCode (and is extensible to others)
* Normalizes each tool’s conversation history into a **unified SQLite schema**
* Builds and maintains a **Tantivy** index (Lucene-like, Rust-native) for sub-50ms “search as you type” over all conversations([GitHub][1])
* Ships via a **`curl | bash` installer** (plus PowerShell equivalent) modeled on the Ultimate Bug Scanner installer, including `--easy-mode` and per-dependency prompts([GitHub][2])

---

## 1. Goals & Non‑Goals

### 1.1 Goals

* **Speed**

  * “Perceived instant” search as you type (<50–80ms for moderate corpora; <200ms for huge ones)
  * Initial indexing amortized via background jobs + incremental updates
* **Coverage**

  * First-class support for:

    * OpenAI **Codex CLI** (terminal agent)([GitHub][3])
    * **Claude Code** (CLI & VS Code extension)([GitHub][4])
    * **Google Gemini CLI** (`gemini-cli`)([GitHub][5])
    * **Amp Code** (Sourcegraph’s Amp CLI)([Amp Code][6])
    * **Cline** (VS Code extension)([Reddit][7])
    * **OpenCode** (opencode-ai/opencode CLI)([HackMD][8])
  * Pluggable architecture to add Cursor CLI, Roo Code, etc. later.
* **UX**

  * Beautiful TUI (ratatui widgets, color themes per agent)([GitHub][9])
  * Hotkeys to filter by time, agent, workspace, project; view full transcript; jump to original log.
* **Portability**

  * Single static(ish) binary per OS; zero runtime deps except libc.
  * Works on:

    * Linux (x86_64, aarch64)
    * macOS (arm64, x86_64)
    * Windows (x86_64, possibly via WSL if some agents are Linux-only).

### 1.2 Non‑Goals / Constraints

* No network calls to remote agent backends (Amp/Claude/Codex clouds). Only **local artifacts** (JSON/JSONL/SQLite) to avoid any auth/privacy issues.
* We don’t attempt to *write back* to these tools’ histories; we only **read and index**.
* Not a general “code search” tool; scope is **chat / agent transcript search**.

---

## 0. LLM-first CLI spec (2025-11)

Context: zero legacy users. Optimize `cass` for AI/automation (tmux/headless), not for human muscle memory. Defaults can change freely.

### Contracts
* CLI is primary; TUI only on explicit `cass tui` (never auto when automation flags present or stdout non-TTY without `tui`).
* Stdout is data-only; stderr is diagnostics/progress.
* Machine error schema: `{"error":{"code":int,"kind":string,"message":string,"hint":string,"retryable":bool}}`.
* Exit codes: 0 ok; 2 usage; 3 missing index/db; 4 network; 5 data-corrupt; 6 incompatible-version; 7 lock/busy; 8 partial; 9 unknown.
* Deterministic defaults printed in help (data dir, db path, log path). Color bars off when non-TTY unless forced.

### Global flags / subcommands
* `--robot-help`: deterministic, wide guide (Summary, Commands, Defaults, Exit codes, JSON/Error schema, Examples, Env, Paths, Trace, Contracts); version header (crate + contract version). No ANSI unless `--color=always`.
* `robot-docs <topic>` topics: `commands`, `env`, `paths`, `schemas`, `exit-codes`, `examples`, `contracts`, `wrap`.
* `--json` everywhere data flows (search/index/etc.).
* `--color=auto|never|always`; default auto (off when non-TTY).
* `--progress=plain|bars|none`; default bars on TTY, plain otherwise.
* `--wrap <cols>` and `--nowrap`; default: no forced wrap (wide output encouraged).
* `--trace-file <path>`: JSONL spans {start_ts,end_ts,duration_ms,cmd,args,exit_code,error?}; never to stdout/stderr.

### Behavioral rules
* If automation flag present (`--json`, `--robot-help`, `robot-docs`, `--trace-file`), TUI path is bypassed; main returns after CLI action.
* If no subcommand and stdout non-TTY: emit short guidance and exit 2 (don’t launch TUI).
* Search pagination: `--limit`, `--offset`, stable ordering.
* Progress bars suppressed by `--quiet` or `--progress=none`; data unaffected.

### Documentation targets
* Embed robot-help content generator; robot-docs topic renders parse-stable blocks.
* README “AI automation” section with wide examples, wrap guidance, trace usage, automation defaults, and no-legacy stance.
* Changelog + version bump; header in robot-help mirrors crate + contract versions.

### Testing targets
* Snapshots for `--robot-help` and each `robot-docs` topic.
* Contract tests: exit codes per scenario, JSON validity, color suppression when non-TTY, wrap flags, TUI bypass, trace file writing.
* Perf sanity: ensure minimal startup overhead for robot-help/doc paths.

---

## 2. Research Summary: Where Each Tool Stores History

This section turns web research into concrete connector requirements.

### 2.1 OpenAI Codex CLI

* **What it is**
  Open-source terminal-native coding agent (`codex` CLI) that reads/edits/runs code locally.([GitHub][3])

* **Config & state locations**

  * Config: `~/.codex/config.toml` (or `$CODEX_HOME/config.toml`)([GitHub][3])
  * Session logs:

    * JSONL “rollout” logs under
      `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (or `$CODEX_HOME/sessions/...`)([GitHub][10])
    * Optionally a consolidated `history.jsonl` in `$CODEX_HOME/history.jsonl` controlled by `history.*` config (e.g. `history.persistence`, `history.max_bytes`).([GitHub][11])

* **Implications for us**

  * Canonical source = **rollout JSONL** files; each describes a session with:

    * Metadata (session id, start time, working directory)
    * User messages / agent steps / approvals / tool runs.
  * We must:

    * Discover `$CODEX_HOME` (env or default `~/.codex`)
    * Recursively scan `sessions/*/*` for `rollout-*.jsonl`.
    * Parse each JSONL line as a “log event” and reconstruct conversations.

### 2.2 Claude Code (CLI + VS Code extension)

* **What it is**
  Anthropic’s agentic coding tool (“Claude Code”) for terminal + editor.

* **History locations (based on ecosystem tools & docs)**
  Community tools for Claude Code history refer to:

  * JSONL session logs under `~/.claude/projects/<project-id>/...`([GitHub][4])
  * Per-project `.claude` / `.claude.json` files in repos for configuration and sometimes embedded logs.([GitHub][12])

* **CLI logs**

  * Several open-source viewers take Claude Code *CLI* logs (JSONL) and render them as Markdown, implying:

    * CLI writes JSONL logs; path varies but `~/.claude/projects` is a strong default.([claude-hub.com][13])

* **Implications**

  * We need a **Claude connector** that:

    * Scans `~/.claude/projects/**` for JSONL files (exclude non-log files).
    * Optionally scans each repo’s `.claude` or `.claude.json` for embedded transcript data.
    * Parses JSONL events into our unified schema.

### 2.3 Gemini CLI (`gemini-cli`)

* **What it is**
  Official Google Gemini CLI for terminal-based workflows.([GitHub][14])

* **History location**

  * A popular “Gemini CLI logs prettifier” script explicitly states:
    **“The Gemini CLI stores chat history and session checkpoints in a series of JSON files located in `~/.gemini/tmp`.”**([GitHub][5])
  * Structure:

    * `~/.gemini/tmp/<project-hash>/checkpoint-*.json`, `chat-log-*.json`, etc.([GitHub][5])

* **Implications**

  * Connector should:

    * Enumerate `~/.gemini/tmp/*` directories.
    * Treat each directory as a project/session cluster.
    * Parse checkpoint & log JSON into conversation threads (ordered by timestamp / sequence).

### 2.4 Amp Code (Sourcegraph Amp CLI)

* **What it is**
  “Frontier coding agent” available as VS Code extension and CLI, built by Sourcegraph.([Amp Code][6])

* **Local storage**

  * Amp mainly stores threads on Sourcegraph servers (Doc & community reports note that “all threads are stored on Sourcegraph servers”).([Reddit][15])
  * VS Code extension:

    * Caches thread history locally under VS Code’s `globalStorage` directory (extension-managed).([Amp Code][16])
  * Amp CLI:

    * Stores credentials in:

      * Linux/macOS: `~/.local/share/amp/secrets.json`
      * Windows: `%APPDATA%\amp\secrets.json`([Amp Code][16])
    * Chat contents themselves are not guaranteed to be fully cached locally.

* **Implications**

  * Our **Amp connector** must:

    * Respect that the **primary truth is remote**; we only index whatever is cached locally:

      * VS Code globalStorage (same pattern as Cline / other extensions).
      * Any CLI cache directories if they exist (we’ll detect by exploring `~/.local/share/amp/` for JSON/JSONL).
    * Provide partial coverage; document clearly in the UI (e.g. label Amp as “local cache only”).

### 2.5 Cline (VS Code task-based coding agent)

* **What it is**
  Popular VS Code extension & ecosystem fork (Roo Code).

* **Local storage**

  * Migration docs & issues consistently point to:

    * macOS Cline data dir:
      `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev`([Reddit][7])
    * Linux analog:
      `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev` (inferred from VS Code layout).
    * Windows analog:
      `%APPDATA%\Code\User\globalStorage\saoudrizwan.claude-dev` (same pattern).
  * In that directory, users mention files like:

    * `taskHistory.json` (index of tasks displayed in “Recent tasks”)
    * One file per task containing:

      * `task_metadata.json`
      * `ui_messages.json`
      * `api_conversation_history.json`([Stack Overflow][17])

* **Implications**

  * Cline connector must:

    * Find the **VS Code globalStorage** dir for the Cline extension.
    * Walk all task directories, reading:

      * `task_metadata.json` → title, created_at, workspace, provider, etc.
      * `ui_messages.json` / `api_conversation_history.json` → actual transcript.
    * Rebuild conversation threads from these JSON files even if `taskHistory.json` is corrupted (StackOverflow questions show that this is needed).([Stack Overflow][17])

### 2.6 OpenCode CLI (opencode-ai/opencode)

* **What it is**
  Local coding agent CLI with MCP support; uses SQLite to persist sessions.([GitHub][18])

* **Storage**

  * Quickstart notes and blog posts describe:

    * On first run, OpenCode creates a `.opencode` **data directory** in the project root and initializes a **SQLite database** for conversation/sessions.([HackMD][8])
    * Config includes a `data.directory` / `database_path` option; default often resides in:

      * Project-local `.opencode`
      * Or a global `~/.config/opencode/...` SQLite file (depending on config).([atalupadhyay][19])

* **Implications**

  * OpenCode connector:

    * Locates per-project `.opencode` directories by scanning:

      * Current git repos (via `git rev-parse --show-toplevel` or just walking up from CWD).
      * `$HOME` for `.opencode` when not inside a repo (optional).
    * Reads SQLite schema (already there), maps `sessions`, `messages`, etc. → our unified schema.

### 2.7 Summary of Paths the App Must Know

Per agent, we need a detection matrix (paths inferred by OS):

| Agent       | Primary history roots (defaults)                                                                                                              |
| ----------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| Codex CLI   | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` (default CODEX_HOME=`~/.codex`); plus `$CODEX_HOME/history.jsonl` if enabled.([GitHub][10]) |
| Claude Code | `~/.claude/projects/**` JSONL logs; plus per-repo `.claude` / `.claude.json`.([GitHub][4])                                                    |
| Gemini CLI  | `~/.gemini/tmp/<project-hash>/{chat,checkpoint}-*.json`.([GitHub][5])                                                                         |
| Amp         | VS Code globalStorage cache for Amp; Amp CLI secrets & any local cache under `~/.local/share/amp` or `%APPDATA%\amp`.([Amp Code][16])         |
| Cline       | VS Code globalStorage: `Code/User/globalStorage/saoudrizwan.claude-dev/**` JSON/JSONL.([Reddit][7])                                           |
| OpenCode    | Project-local `.opencode` directories with SQLite DB; global `~/.config/opencode/...` if configured.([HackMD][8])                             |

---

## 3. Core Architecture

### 3.1 Top-level Components

1. **CLI / entrypoint** (`main.rs`)

   * Subcommands:

     * `agent-search tui` (default): launch full-screen TUI.
     * `agent-search index`:

       * `--full`: rebuild entire index from scratch.
       * `--incremental`: only new or changed logs.
     * `agent-search inspect <agent> <session-id>`: dump normalized view of a single conversation.

2. **Connectors layer** (`connectors::*`)

   * One module per agent:

     * `connectors::codex`
     * `connectors::claude_code`
     * `connectors::gemini`
     * `connectors::amp`
     * `connectors::cline`
     * `connectors::opencode`
   * Each exposes:

     * Detection:

       ```rust
       fn detect_installation(env: &Environment) -> DetectionResult;
       ```
     * Scan & normalize:

       ```rust
       fn scan_sessions(ctx: &ScanContext) -> anyhow::Result<Vec<NormalizedConversation>>;
       fn watch_paths(ctx: &ScanContext, tx: Sender<IndexUpdate>) -> anyhow::Result<()>;
       ```

3. **Data model & persistence** (`model`, `storage`)

   * `model` defines normalized Rust structs for:

     * `Agent`, `Conversation`, `Message`, `Snippet`, `Workspace`.
   * `storage::sqlite`

     * SQLite DB (rusqlite) with strongly-typed schema.([Docs.rs][20])

4. **Search engine** (`search`)

   * Primary index: **Tantivy** (Lucene-like).([GitHub][1])
   * Secondary / fallback: SQLite FTS5 virtual table.([SQLite][21])

5. **TUI / UI** (`ui`)

   * Built with Ratatui + `ratatui-crossterm` backend.([GitHub][9])

6. **Index orchestrator** (`indexer`)

   * Coordinates:

     * Initial full scan
     * Incremental updates (filesystem watchers via `notify`)([GitHub][22])
     * Rebuilding indexes when schema changes.

7. **Config** (`config`)

   * YAML/TOML config stored in XDG / platform-appropriate directories via `directories` crate.([Crates][23])

8. **Logging & error handling**

   * `tracing` + `tracing-subscriber` for logging.
   * `color-eyre` or `miette` for pretty diagnostics in CLI mode.([The Rust Programming Language Forum][24])

### 3.2 Process Model & Threads

* **UI thread**

  * Runs the Ratatui event loop, processes user input (crossterm events).
* **Search worker pool**

  * Uses `rayon` to parallelize search + scoring over Tantivy index.([Crates][25])
* **Index worker**

  * Thread that:

    * Listens for `IndexUpdate` messages from:

      * Connectors (full/partial scans)
      * Filesystem watchers (notify)
    * Batch-writes to SQLite & Tantivy.

Communication via `crossbeam::channel`:

```rust
enum UiEvent { Key(KeyEvent), Tick, SearchResult(SearchResults) }
enum IndexCommand { FullReindex, IncrementalScan, FilesystemEvent(FsEvent) }

struct Channels {
    ui_tx: Sender<UiEvent>, ui_rx: Receiver<UiEvent>,
    index_tx: Sender<IndexCommand>, index_rx: Receiver<IndexCommand>,
}
```

---

## 4. Unified Data Model & SQLite Schema

### 4.1 Conceptual Model

* **Agent**: `codex`, `claude_code`, `gemini_cli`, `amp`, `cline`, `opencode`, …
* **Workspace**: root path of repo / project (if known).
* **Conversation**: one “thread” or “task”.
* **Message**: user or agent message, plus tool runs / actions.
* **Snippet**: optional code snippet or file section references.

### 4.2 SQLite Schema (normalized, tuned for performance)

We’ll create a single SQLite DB under app data dir:

* Use `rusqlite` with `bundled` feature to ship our own SQLite build (ensures FTS5 is available across platforms).([Docs.rs][20])

**Tables**

```sql
-- Agents (tools)
CREATE TABLE agents (
    id              INTEGER PRIMARY KEY,
    slug            TEXT NOT NULL UNIQUE,   -- "codex", "cline", etc.
    name            TEXT NOT NULL,
    version         TEXT,
    kind            TEXT NOT NULL,         -- "cli", "vscode", "hybrid"
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

-- Workspaces (projects / repos)
CREATE TABLE workspaces (
    id              INTEGER PRIMARY KEY,
    path            TEXT NOT NULL,         -- canonical absolute path
    display_name    TEXT,
    UNIQUE(path)
);

-- Conversations (threads / tasks)
CREATE TABLE conversations (
    id              INTEGER PRIMARY KEY,
    agent_id        INTEGER NOT NULL REFERENCES agents(id),
    workspace_id    INTEGER REFERENCES workspaces(id),
    external_id     TEXT,                  -- tool's session/task ID
    title           TEXT,
    source_path     TEXT NOT NULL,         -- original log / DB path
    started_at      INTEGER,               -- unix millis
    ended_at        INTEGER,
    approx_tokens   INTEGER,
    metadata_json   TEXT,                  -- extra tool-specific info
    UNIQUE(agent_id, external_id)
);

-- Messages
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    idx             INTEGER NOT NULL,      -- order in conversation
    role            TEXT NOT NULL,         -- "user","agent","tool","system"
    author          TEXT,
    created_at      INTEGER,               -- may be null if unknown
    content         TEXT NOT NULL,
    extra_json      TEXT
);

-- Optional per-message code snippets / file refs
CREATE TABLE snippets (
    id              INTEGER PRIMARY KEY,
    message_id      INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    file_path       TEXT,
    start_line      INTEGER,
    end_line        INTEGER,
    language        TEXT,
    snippet_text    TEXT
);

-- Simple tag layer (for later)
CREATE TABLE tags (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE
);

CREATE TABLE conversation_tags (
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    tag_id          INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (conversation_id, tag_id)
);
```

### 4.3 SQLite Performance Tuning

On DB open, apply pragmas:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;       -- or FULL for "safe mode"
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -65536;        -- 64MB in pages
PRAGMA foreign_keys = ON;
PRAGMA mmap_size = 268435456;      -- 256MB (tuneable)
```

Indexes:

```sql
CREATE INDEX idx_conversations_agent_started
    ON conversations(agent_id, started_at DESC);

CREATE INDEX idx_messages_conv_idx
    ON messages(conversation_id, idx);

CREATE INDEX idx_messages_created
    ON messages(created_at);
```

### 4.4 SQLite FTS5 Mirror

To support fallback search (and some advanced filters), create an FTS5 virtual table:

```sql
CREATE VIRTUAL TABLE fts_messages
USING fts5(
    content,
    title,
    agent_slug,
    workspace,
    message_id UNINDEXED,
    conversation_id UNINDEXED,
    created_at UNINDEXED,
    tokenize = "porter"
);
```

We then keep `fts_messages` synchronized with `messages` via our Rust code (not triggers, to avoid performance surprises).

FTS5 gives fast text search, built-in ranking, and helps on platforms where Tantivy or its index is temporarily unavailable.([SQLite][21])

---

## 5. Search Engine (Tantivy) Design

### 5.1 Why Tantivy

* Tantivy is a **Rust-native Lucene-like full-text search engine** with high performance and a feature set comparable to Elasticsearch’s core text features.([GitHub][1])
* Well suited as “Rust equivalent to Lucene/Elastic” per the requirement.

### 5.2 Index Layout

**Index location**

* On-disk under app data dir: `data_dir/index/` (per schema version, e.g. `index/v1/`).

**Fields**

* `message_id` (u64, stored)
* `conversation_id` (u64, stored)
* `agent_slug` (string, indexed, fast field for filters)
* `workspace` (string, indexed)
* `created_at` (i64, indexed as fast field, sortable)
* `role` (string, indexed)
* `title` (text, indexed & stored)
* `content` (text, indexed & stored)

Use:

* `TEXT` fields with a standard analyzer (tokenization, lowercasing, stopwords).
* `FAST` fields for `created_at` and `agent_slug` to support efficient range & term filters.

### 5.3 Query Model

When user types into search box:

1. Build a Tantivy query:

   * `Query::Boolean` combining:

     * Full-text query on `content` & `title`

       * Multi-field query parser with weights:

         * `title` weight 3.0
         * `content` weight 1.0
     * Agent filter(s): `TermQuery` on `agent_slug`.
     * Time filter: `RangeQuery` on `created_at`.
2. Limit: top 100 hits (configurable).
3. Group results by conversation for TUI display:

   * Each conversation row shows best-scoring message snippet.

### 5.4 Performance

* Pre-open a Tantivy `IndexReader` & `Searcher` on startup.
* Use Tantivy’s multi-threaded search (via its internal threadpool) plus `rayon` for grouping and post-processing.([quickwit.io][26])
* Debounce keystroke-triggered searches by ~100–150ms:

  * Send `SearchRequest { query, filters, timestamp }` on each change.
  * Worker deduplicates by dropping stale requests.

---

## 6. TUI Design

### 6.1 Libraries & Terminal Handling

* TUI: `ratatui` for widgets/layout.([GitHub][9])
* Backend: `ratatui-crossterm` using `crossterm` for cross-platform terminal control.([Crates][27])

### 6.2 Layout

Main screen = 3 panes:

1. **Top bar** (1–2 rows)

   * Search input `[ query here… ]`
   * Filter summary:

     * `Agents: Codex, Claude, Gemini`
     * `Time: Last 7 days`
     * `Workspace: all`
   * Right side: status (indexing progress, #docs, backend used: Tantivy/FTS)

2. **Left pane – Results list**

   * Scrollable list of hit conversations/messages.
   * Each row:

     * `[AGENT ICON] [REL TIME] [WORKSPACE] Title / first line snippet`
   * Colored by agent:

     * Codex: cyan
     * Claude: purple
     * Gemini: blue
     * Amp: magenta
     * Cline: green
     * OpenCode: yellow

3. **Right pane – Detail**

   * When a row is selected:

     * Shows full conversation:

       * Timestamped
       * Roles (“You”, “Agent”, “Tool”) with colors.
   * Tabs at top: `[Messages] [Code Snippets] [Raw JSON]`.

Bottom status line:

* Hints: `Enter: open | /: search | f: filters | t: time | a: agents | w: workspace | ?: help | q: quit`.

### 6.3 Hotkeys

* Navigation:

  * `↑/↓` or `k/j` – move selection in result list.
  * `PgUp/PgDn` – page results.
  * `Tab` – toggle focus between search box / results / detail.
* Search:

  * `/` – focus search input.
  * `Esc` – clear search if input nonempty, else go back/focus results.
* Filters:

  * `f` – open filter popover.
  * `a` – agent filter:

    * Checkbox list of agents; space toggles; enter applies.
  * `t` – time filter modal:

    * Quick presets: `1` = last 24h, `7` = last 7 days, `3` = last 30 days, `0` = all.
    * `c` = custom; prompts from/to dates.
  * `w` – workspace filter:

    * List of detected workspaces; search-as-you-type fuzzy filter (local to this list).
* Detail:

  * `Enter` – open conversation in detail pane (if not already).
  * `o` – open underlying log/DB in external editor (`$EDITOR + path:line`).
  * `r` – toggle between “grouped by turn” vs “flat log” view.

### 6.4 Styling & Polish

* Use Ratatui’s `Block`, `List`, `Paragraph`, `Tabs` widgets with:

  * Light borders, rounded corners where available.
  * Highlight style for selected row: reverse video + bold.
  * Soft accent colors rather than neon; calibrate for readability in dark mode.
* Support light/dark themes via config (`theme = "dark" | "light"`).
* Optional “minimal mode” that disables some borders for simpler terminals.

---

## 7. Connectors: Detailed Behavior

### 7.1 Shared Connector Abstractions

Define:

```rust
struct NormalizedConversation {
    agent_slug: String,
    external_id: String,
    title: Option<String>,
    workspace: Option<PathBuf>,
    source_path: PathBuf,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    metadata: serde_json::Value,
    messages: Vec<NormalizedMessage>,
}

struct NormalizedMessage {
    role: MessageRole,
    author: Option<String>,
    created_at: Option<i64>,
    content: String,
    extra: serde_json::Value,
    snippets: Vec<NormalizedSnippet>,
}
```

Each connector:

* Emits `NormalizedConversation` objects.
* Does **idempotent** scans: uses `source_path` + `external_id` to avoid duplicates.

### 7.2 Codex Connector

**Detection**

* Check:

  * Is `codex` binary on PATH? Use `which` crate to detect executables robustly across platforms.([Stack Overflow][28])
  * Or does `~/.codex` exist?

**Scan**

* Determine `$CODEX_HOME`:

  * Env var `CODEX_HOME` or default `~/.codex`.
* Enumerate `sessions`:

  * `CODEX_HOME/sessions/*/*/rollout-*.jsonl`
* For each `rollout-*.jsonl`:

  * Treat file as one “session”.
  * Parse JSONL line-by-line:

    * Identify user messages vs agent messages (look at event type: `user_message`, `assistant_message`, etc.).
    * Extract timestamps, workspace path, title (if present), approvals, tool runs.
  * Build `NormalizedConversation`:

    * `external_id` = file path or session UUID from JSON.
    * `workspace` = working directory from session metadata.
    * `started_at` = first event timestamp; `ended_at` = last.
* Optionally, incorporate `history.jsonl`:

  * As fallback when sessions missing; but primary will be session logs.

**Incremental updates**

* Use `notify` to watch:

  * `$CODEX_HOME/sessions` directory for new/changed files.
* On new `rollout-*.jsonl`:

  * Parse, upsert in DB and update Tantivy/FTS.

### 7.3 Claude Code Connector

**Detection**

* Heuristics:

  * `~/.claude` directory exists.
  * VS Code extension for Claude installed (look for `claude-code` or similar in `globalStorage` directories).
* Config-driven override:

  * Allow user to specify `claude.projects_dir` etc.

**Scan**

* Root: `~/.claude/projects`.
* For each project dir:

  * List `.jsonl` history logs (names may vary: `history-*.jsonl`, `session-*.jsonl`).
  * Parse JSONL:

    * Each line = event. Identify conversation boundaries (session-id field).
* Map fields:

  * Title: may come from “task name” or first user message.
  * Role: map Claude’s `user`, `assistant`, `tool`.
  * Workspace: if path is embedded; else null.
* Additionally, check per-repo `.claude` / `.claude.json`:

  * Some setups store “project memory” or limited history there; treat as additional conversations.

**Incremental**

* Watch `~/.claude/projects` for new/updated `.jsonl` files.

### 7.4 Gemini CLI Connector

**Detection**

* `gemini` binary on PATH (`which "gemini"` or `gemini-cli`), or `~/.gemini` directory.([GitHub][5])

**Scan**

* Root: `~/.gemini/tmp`.
* For each child dir `<project-hash>`:

  * Enumerate JSON files:

    * `checkpoint-*.json`, `chat-*.json`, etc.
* Reconstruction strategy (from logs-prettifier script semantics):([GitHub][5])

  * Checkpoints contain “current conversation state”; chat logs contain message history.
  * Prefer chat logs; if absent, fallback to checkpoints.
* Build:

  * `external_id` = directory name + checkpoint id.
  * `title` = derived from first user message or model-provided session name.
  * Timestamps = earliest / latest message timestamps.

**Incremental**

* Watch `~/.gemini/tmp` for new directories / files.

### 7.5 Amp Connector

**Detection**

* `amp` CLI on PATH (`npm i -g @sourcegraph/amp` installs it).([marketplace.visualstudio.com][29])
* `~/.local/share/amp` exists or `%APPDATA%\amp`.([Amp Code][16])

**Scan**

* Local thread storage is **limited** (most sessions remote).([Reddit][15])
* Strategy:

  * Inspect `~/.local/share/amp` / `%APPDATA%\amp`:

    * Any JSON/JSONL logs? (we’ll define a naming convention once we see typical installs).
  * Inspect VS Code globalStorage for Amp extension (similar to Cline):

    * e.g. `Code/User/globalStorage/sourcegraph.amp/**`.
* If we find JSON/JSONL per thread:

  * Map them to `NormalizedConversation`.
* Tag Amp conversations as `partial = true` in metadata.

### 7.6 Cline Connector

**Detection**

* Check for VS Code globalStorage path:

  * Platform-specific pattern resolving to `<vscode-config>/User/globalStorage/saoudrizwan.claude-dev`.([Reddit][7])

**Scan**

* In that directory:

  * Identify per-task directories or files:

    * `taskHistory.json` summarizing tasks (maybe optional if corrupted).([Stack Overflow][17])
    * A directory per task UUID with:

      * `task_metadata.json`
      * `ui_messages.json`
      * `api_conversation_history.json`
  * If `taskHistory.json` exists:

    * Use as index for tasks (title, created_at).
  * For each task:

    * Parse `task_metadata.json`:

      * Title, provider, workspace root, etc.
    * Parse `ui_messages.json` / `api_conversation_history.json`:

      * Build ordered message list; unify user vs agent vs tool roles.
* `external_id` = task id.

**Incremental**

* Watch globalStorage dir for changes.

### 7.7 OpenCode Connector

**Detection**

* On startup:

  * If `opencode` CLI on PATH (nice but not required).
  * Scan:

    * Current working dir upward for `.opencode` (project-local).
    * `$HOME` for `.opencode` or config-specified global DB.([HackMD][8])

**Scan**

* For each `.opencode` dir:

  * Read config (if present) to locate SQLite DB.
  * Open DB with rusqlite and introspect schema.

    * Likely tables: `sessions`, `messages`, `files`, etc.
* Map:

  * Each row in `sessions` = `Conversation`.
  * Each `message` row = `Message`.
  * Additional tables (e.g., `files`) → `Snippet`s or tags.

**Incremental**

* For SQLite DB, we can’t easily watch per-row changes, but we can:

  * Track DB `mtime` and last imported row id / timestamp per DB.
  * On change:

    * Query for rows newer than last imported.

---

## 8. Indexer & Synchronization Flow

### 8.1 First Run

1. User runs `agent-search` (TUI command).
2. App locates config dir (`directories::ProjectDirs` for `coding_agent_search`).([Crates][23])
3. If DB / index missing:

   * Run **initial detection**:

     * For each connector, call `detect_installation`.
   * Show small TUI dialog:

     * “Detected: Codex, Cline, Gemini. Index now? [Yes] [Skip]”
   * Kick off **full scan** in background thread:

     * Progress bar in status bar:

       * “Indexing Codex: 327/1024 sessions…”

### 8.2 Incremental Updates

* For log-file-based sources:

  * Use `notify` watchers on root dirs (`~/.codex`, `~/.gemini/tmp`, `~/.claude/projects`, VS Code globalStorage).([GitHub][22])
  * Debounce FS events to avoid thrashing.
* For SQLite-based sources (OpenCode):

  * Periodic polling (e.g., every 60s) for DB `mtime` change.
* On new/changed source:

  * Re-run corresponding connector `scan_sessions` but with:

    * `since_timestamp` = last import time per source file / DB.

### 8.3 Schema Migrations

* Maintain `schema_version` in a small `meta` table.
* On binary upgrade:

  * If schema mismatch:

    * Run migration scripts (Rust-implemented).
    * Optionally rebuild Tantivy index from SQLite.

---

## 9. Installer Design (`curl | bash`)

### 9.1 Goals

* Single-line install inspired by Ultimate Bug Scanner:([GitHub][2])

```bash
curl -fsSL https://raw.githubusercontent.com/<you>/coding-agent-search/main/install.sh | bash
```

* Support `--easy-mode` to:

  * Auto-install all dependencies without prompting.
  * Auto-enable all detected agents.

### 9.2 Install Script Behavior (Linux/macOS)

**1. Safety & prerequisites**

* `set -euo pipefail`
* Check for:

  * `curl` or `wget`
  * `tar`
  * `uname`, `mktemp`
* Print what it will do and ask confirmation (unless `--easy-mode`).

**2. Detect OS / arch**

* `uname -s` → `Linux` / `Darwin`.
* `uname -m` → `x86_64` / `arm64`.

**3. Download binary**

* Determine latest version (github releases API or static `VERSION` file).
* Download `agent-search-<os>-<arch>.tar.gz`.
* Verify checksum (SHA-256 baked into script; like UBS does for its modules).([GitHub][2])

**4. Install location**

* Default: `${HOME}/.local/bin/agent-search` (or `~/bin` fallback).
* Optionally `/usr/local/bin` if user chooses and has sudo.

**5. Dependencies**

We aim to build a **fully self-contained** binary (bundled SQLite, static linking), so external dependencies are minimal. For extra tools we might optionally use:

* `sqlite3` CLI (for debug)
* `less` or `bat` (for external viewing)([GitHub][30])

Script logic:

* Detect package manager: `apt`, `dnf`, `pacman`, `brew`, `yum`, `zypper`.
* For each missing extra:

  * Prompt: “Install sqlite3 with apt? [Y/n]” unless `--easy-mode`.

**6. Post-install**

* Add `${HOME}/.local/bin` to PATH if missing (touch shell rc).
* Print quickstart:

```text
Run: agent-search
Or:  agent-search tui
```

### 9.3 Windows Installer (PowerShell)

* Equivalent PowerShell command:

```powershell
irm https://raw.githubusercontent.com/<you>/coding-agent-search/main/install.ps1 | iex
```

* Steps:

  * Detect architecture via `[Environment]::Is64BitOperatingSystem`.
  * Download `agent-search-windows-x86_64.zip`.
  * Extract to `%LOCALAPPDATA%\Programs\agent-search`.
  * Add that directory to user PATH (via registry or `setx`).
* For Windows lacking proper terminal support:

  * Recommend **Windows Terminal** or WSL; but the binary should still work with standard console.

---

## 10. Agent Auto-Detection Strategy

### 10.1 Executable Detection

Use `which` or `pathsearch` crate to reliably find executables in PATH on all OSes (handles PATHEXT on Windows).([Stack Overflow][28])

* Binaries to probe:

  * `codex`
  * `amp`
  * `gemini` or `gemini-cli`
  * `opencode`
  * For Claude Code / Cline (more VSCode-embedded), detection will lean on filesystem directories.

### 10.2 Filesystem Heuristics

* Check for each tool's canonical conf/data dirs (see Section 2 table).
* If path exists **and** contains expected “signature file”:

  * Codex: `~/.codex/config.toml`([GitHub][3])
  * Gemini: `~/.gemini/tmp` with `checkpoint-*.json`.([GitHub][5])
  * Claude: `~/.claude/projects` with JSONL.([GitHub][4])
  * Cline: VS Code globalStorage dir with `taskHistory.json`.([Reddit][7])
  * Amp: `~/.local/share/amp/secrets.json` or `%APPDATA%\amp\secrets.json`.([Amp Code][16])
  * OpenCode: `.opencode` directories / global config.([HackMD][8])

### 10.3 User-facing Detection UI

On first run (and accessible via `Settings`):

* Show list:

| Agent       | Detected? | Evidence                           | Enabled? |
| ----------- | --------- | ---------------------------------- | -------- |
| Codex CLI   | yes/no    | `codex` in PATH, `~/.codex/...`    | [x]      |
| Claude Code | yes/no    | `~/.claude/projects`               | [x]      |
| Gemini CLI  | yes/no    | `~/.gemini/tmp`                    | [x]      |
| Amp         | yes/no    | `amp` CLI, Amp globalStorage, etc. | [ ]      |
| Cline       | yes/no    | VSCode globalStorage dir           | [x]      |
| OpenCode    | yes/no    | `.opencode` dirs / global DB       | [x]      |

User can toggle connectors on/off; this is stored in config.

---

## 11. Configuration

### 11.1 Config File Layout

* Use `directories::ProjectDirs` to compute platform-correct config directory, e.g.:

  * Linux: `~/.config/coding-agent-search/config.toml`
  * macOS: `~/Library/Application Support/coding-agent-search/config.toml`
  * Windows: `%APPDATA%\coding-agent-search\config.toml`([Crates][23])

Example `config.toml`:

```toml
[general]
theme = "dark"
enable_tantivy = true
max_results = 200

[sqlite]
path = "/home/user/.local/share/coding-agent-search/agent_search.db"
page_size = 4096
cache_size_mb = 64

[agents.codex]
enabled = true
home = "/home/user/.codex"

[agents.claude_code]
enabled = true
projects_dir = "/home/user/.claude/projects"

[agents.gemini_cli]
enabled = true
root = "/home/user/.gemini/tmp"

[agents.amp]
enabled = false
note = "Limited to local cache only"

[agents.cline]
enabled = true
vscode_profile = "Code"         # or "Code - Insiders"

[agents.opencode]
enabled = true
search_project_roots = true
extra_db_paths = []
```

### 11.2 Advanced Tuning Options

* `tantivy.index_path`, `tantivy.num_indexing_threads`.
* `search.default_time_range` (e.g., `7d`).
* `search.min_query_length` for search-as-you-type.
* `performance.max_conversations` to index; can be unlimited by default.

---

## 12. Testing & Benchmarking Plan

### 12.1 Unit Tests

* For each connector:

  * Synthetic minimal log/DB sample → normalized conversations.
  * Backwards-compat as upstream tools change (guard by snapshot tests).
* For SQLite:

  * Schema migrations tested with up/down simulation.
* For search:

  * Queries returning expected conversations for various filters.

### 12.2 Integration Tests

* End-to-end:

  1. Spin up a temp dir as “home”.
  2. Place sample logs for Codex, Cline, Gemini, etc.
  3. Run `agent-search index --full --config test-config.toml`.
  4. Run `agent-search tui` in non-interactive mode:

     * Feed keystrokes.
     * Assert on output (e.g., via `crossterm` recording or snapshotting RAT).

### 12.3 Performance Benchmarks

Baseline dataset: e.g.,

* 10k conversations
* 1M messages
* Several hundred MB raw logs.

Metrics:

* Full index time with Tantivy only, SQLite only, both.
* Search latency distribution vs query length and filter complexity.
* Memory footprint vs dataset size.

Use `criterion` for benchmark harness.

---

## 13. Roadmap

### 13.1 v0 (MVP)

* CLI & TUI skeleton (Ratatui + crossterm).
* SQLite storage with schema above.
* Tantivy index:

  * Simple indexing of `content`, `title`, `agent_slug`, `created_at`.
* Connectors:

  * Codex CLI (session logs)
  * Cline (VS Code globalStorage)
  * Gemini CLI (`~/.gemini/tmp`)
* Installer:

  * `install.sh` (curl | bash) for Linux/macOS.
  * Manual install instructions for Windows.

### 13.2 v1

* Full connectors:

  * Claude Code (global projects & `.claude` files)
  * OpenCode (SQLite integration)
  * Initial Amp support (local caches only).
* `notify`-based incremental indexer.
* Filter UI (per-agent, time range, workspace).
* Config file + dynamic reload (`r` to reload config).

### 13.3 v2+

* Better Amp & Claude Code support as they stabilize history APIs.
* Export features:

  * `agent-search export --agent codex --format jsonl` etc.
* “Session merge” features:

  * Combine related threads from different tools for the same repo.
* Optional vector-embedding index layered on top of Tantivy/FTS for semantic search.

---

## 14. Concrete Implementation Checklist

A very granular build order to actually implement this:

1. **Scaffolding**

   * `cargo new coding-agent-search`
   * Add dependencies:

     * `ratatui`, `crossterm`, `ratatui-crossterm`
     * `rusqlite` (with `bundled` + `modern_sqlite` features)([Docs.rs][20])
     * `tantivy`
     * `serde`, `serde_json`, `serde_yaml`, `toml`
     * `directories-next` or `directories`
     * `notify`
     * `rayon`
     * `clap` (derive)([Docs.rs][31])
     * `which`
     * `tracing`, `tracing-subscriber`
     * `color-eyre` or `miette`.

2. **Core modules**

   * Implement `config` with load/save and defaults.
   * Implement `storage::sqlite`:

     * DB initialization, pragmas, migrations.
   * Implement `search::tantivy`:

     * schema, index writer, searcher.

3. **Minimal TUI**

   * Basic layout (search bar + list + detail).
   * Hard-coded dummy data for results.

4. **Codex connector**

   * Env detection, path scanning.
   * Minimal JSONL parsing and mapping into DB/index.

5. **Cline connector**

   * VS Code path resolution per OS.
   * Task directory parsing.

6. **Gemini connector**

   * `~/.gemini/tmp` scanning and JSON parsing.

7. **Index orchestration**

   * Full `index` command.
   * TUI-triggered incremental reindex.

8. **Installer**

   * Implement `install.sh` copying patterns from UBS (easy mode, sha256 verification, module detection).([GitHub][2])
   * Add GitHub workflow to build release tarballs/zips.

9. **Remaining connectors**

   * Claude Code, OpenCode, Amp.

10. **Polish**

    * Theming, help screen, keybinding docs.
    * Config toggle for FTS vs Tantivy.
    * Extensive tests and benchmarks.

---

This plan should be enough to sit down and start coding the entire system in Rust, with each piece grounded in how the underlying tools really store their histories and in current best practices for Rust TUIs, embedded search, and installer UX.

[1]: https://github.com/quickwit-oss/tantivy?utm_source=chatgpt.com "Tantivy is a full-text search engine library inspired ... - GitHub"
[2]: https://github.com/Dicklesworthstone/ultimate_bug_scanner "GitHub - Dicklesworthstone/ultimate_bug_scanner:  Industrial-grade static analysis for all popular programming languages. Catch 1000+ bug patterns before production"
[3]: https://github.com/openai/codex?utm_source=chatgpt.com "openai/codex: Lightweight coding agent that runs in your ..."
[4]: https://github.com/jhlee0409/claude-code-history-viewer?utm_source=chatgpt.com "jhlee0409/claude-code-history-viewer"
[5]: https://github.com/google-gemini/gemini-cli/discussions/3965 "A script to visualize and prettify the logged chats - ready script · google-gemini gemini-cli · Discussion #3965 · GitHub"
[6]: https://ampcode.com/?utm_source=chatgpt.com "Amp"
[7]: https://www.reddit.com/r/CLine/comments/1l1u7hb/migrating_to_new_macbook/?utm_source=chatgpt.com "Migrating to new MacBook - CLine"
[8]: https://hackmd.io/%40dps/Hkm5VA06le?utm_source=chatgpt.com "Install OpenCode and Configure z.ai as a Custom Model"
[9]: https://github.com/ratatui/ratatui?utm_source=chatgpt.com "ratatui/ratatui: A Rust crate for cooking up terminal user ..."
[10]: https://github.com/openai/codex/discussions/2956?utm_source=chatgpt.com "Save chat/history in the VS Code Codex extension #2956"
[11]: https://github.com/openai/codex/issues/4963?utm_source=chatgpt.com "\"Log rotate\" CODEX_HOME/history.jsonl · Issue #4963"
[12]: https://github.com/anthropics/claude-code/issues/5024?utm_source=chatgpt.com "History accumulation in .claude.json causes performance ..."
[13]: https://www.claude-hub.com/resource/github-cli-withLinda-claude-JSONL-browser-claude-JSONL-browser/?utm_source=chatgpt.com "claude-JSONL-browser | Claude Code Resource"
[14]: https://github.com/google-gemini/gemini-cli/issues/3882?utm_source=chatgpt.com "Automatically save chat history · Issue #3882"
[15]: https://www.reddit.com/r/cursor/comments/1kpin6e/tried_amp_sourcegraphs_new_ai_coding_agent_heres/?utm_source=chatgpt.com "Tried Amp, Sourcegraph's new AI coding agent"
[16]: https://ampcode.com/security?utm_source=chatgpt.com "Security Reference"
[17]: https://stackoverflow.com/questions/79807883/cline-ai-extension-history-not-loading-in-vs-code-empty-taskhistory-json-and?utm_source=chatgpt.com "Empty taskHistory.json and large task context (>10MB)"
[18]: https://github.com/WismutHansen/lst?utm_source=chatgpt.com "WismutHansen/lst: Personal notes, todos, lists etc without any bloat"
[19]: https://atalupadhyay.wordpress.com/2025/06/16/open-code-building-your-ultimate-terminal-based-ai-coding-assistant/?utm_source=chatgpt.com "Building Your Ultimate Terminal-Based AI Coding Assistant"
[20]: https://docs.rs/rusqlite/?utm_source=chatgpt.com "rusqlite - Rust"
[21]: https://sqlite.org/fts5.html?utm_source=chatgpt.com "SQLite FTS5 Extension"
[22]: https://github.com/notify-rs/notify?utm_source=chatgpt.com "notify-rs/notify: 🔭 Cross-platform filesystem notification ..."
[23]: https://crates.io/crates/directories?utm_source=chatgpt.com "directories - crates.io: Rust Package Registry"
[24]: https://users.rust-lang.org/t/miette-vs-anyhow-color-eyre/110197?utm_source=chatgpt.com "Miette vs anyhow/(color-)eyre - help"
[25]: https://crates.io/crates/rayon?utm_source=chatgpt.com "rayon - crates.io: Rust Package Registry"
[26]: https://quickwit.io/blog/tantivy-0.21?utm_source=chatgpt.com "Tantivy 0.21"
[27]: https://crates.io/crates/ratatui-crossterm?utm_source=chatgpt.com "ratatui-crossterm - crates.io: Rust Package Registry"
[28]: https://stackoverflow.com/questions/37498864/finding-executable-in-path-with-rust?utm_source=chatgpt.com "Finding executable in PATH with Rust"
[29]: https://marketplace.visualstudio.com/items?itemName=sourcegraph.amp&utm_source=chatgpt.com "Amp (Research Preview)"
[30]: https://github.com/sharkdp/bat?utm_source=chatgpt.com "sharkdp/bat: A cat(1) clone with wings."
[31]: https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html?utm_source=chatgpt.com "clap::_derive::_tutorial - Rust"
