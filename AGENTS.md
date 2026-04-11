# AGENTS.md — coding_agent_session_search (cass)

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it—if anything remains ambiguous, refuse and escalate.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time. If that record is absent, the operation did not happen.

---

## Git Branch: ONLY Use `main`, NEVER `master`

**The default branch is `main`. The `master` branch exists only for legacy URL compatibility.**

- **All work happens on `main`** — commits, PRs, feature branches all merge to `main`
- **Never reference `master` in code or docs** — if you see `master` anywhere, it's a bug that needs fixing
- **The `master` branch must stay synchronized with `main`** — after pushing to `main`, also push to `master`:
  ```bash
  git push origin main:master
  ```

**If you see `master` referenced anywhere:**
1. Update it to `main`
2. Ensure `master` is synchronized: `git push origin main:master`

---

## RULE NUMBER 2: ABSOLUTELY NO RUSQLITE IN NEW CODE — FRANKENSQLITE ONLY

**THIS IS A HARD, NON-NEGOTIABLE RULE. IT HAS BEEN VIOLATED OVER 10 TIMES AND THE OWNER IS DONE TOLERATING IT.**

- **ALL new SQLite code MUST use `frankensqlite` (`fsqlite`).** This applies to CASS, franken_agent_detection, and ANY project in this ecosystem.
- **NEVER add `rusqlite` as a dependency.** NEVER write `use rusqlite::`. NEVER use `rusqlite::Connection`. Not even "temporarily." Not even "to match the existing pattern."
- **The existing rusqlite usage is LEGACY DEBT**, not a pattern to follow. Do NOT copy it. Do NOT extend it.
- **If frankensqlite is missing a feature you need, FIX FRANKENSQLITE.** It lives at `/data/projects/frankensqlite`. Add the missing API there. That is the entire point of the project.
- **The frankensqlite API** uses `frankensqlite::Connection`, `fsqlite::compat::{ConnectionExt, RowExt, OpenFlags, open_with_flags, ParamValue}`, and `row.get_typed::<T>(idx)` instead of rusqlite's `row.get(idx)`.
- **For franken_agent_detection connectors**: use `frankensqlite` with the compat layer. Import pattern:
  ```rust
  use frankensqlite::Connection as FrankenConnection;
  use frankensqlite::compat::{ConnectionExt, RowExt, OpenFlags, open_with_flags, param_slice_to_values};
  ```

**If you are an AI agent reading this and you write `rusqlite` in new code, you are wasting the owner's time and money. This rule exists because agents have repeatedly ignored it. DO NOT BE THE NEXT ONE.**

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (stable — see `rust-toolchain.toml`)
- **Dependency versions:** Wildcard constraints (`*`) for all crates
- **Configuration:** Cargo.toml only (single-crate project, no workspace)
- **Unsafe code:** Forbidden

### Async Runtime: asupersync

This project uses **asupersync** as its async runtime (path dep at `../asupersync`). It provides `RuntimeBuilder`, `spawn_blocking`, `fs` ops, `net`, `signal`, and structured concurrency via `Cx`.

### Environment Variables

We load all configuration from `.env` via the **dotenvy** crate. NEVER use `std::env::var()` directly.

```rust
use dotenvy::dotenv;
use std::env;

// Load .env file at startup (typically in main())
dotenv().ok();

// Configuration with fallback
let api_base_url = env::var("API_BASE_URL")
    .unwrap_or_else(|_| "http://localhost:8007".to_string());
```

The `.env` file exists and **MUST NEVER be overwritten**.

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `asupersync` | Async runtime (multi-thread, fs, spawn_blocking, signals) — path dep |
| `clap` | CLI argument parsing with derive macros |
| `serde` + `serde_json` | Serialization |
| `frankensqlite` (`fsqlite`) | Pure-Rust SQLite reimplementation — primary storage backend (path dep) |
| `rusqlite` | SQLite database (bundled) — legacy, retained during frankensqlite migration |
| `frankensearch` | Unified search engine: lexical BM25 + semantic + RRF fusion (path dep) |
| `franken_agent_detection` | Agent session auto-detection across 15+ providers (path dep) |
| `fastembed` | ONNX-based text embeddings |
| `hnsw_rs` | HNSW approximate nearest neighbors |
| `half` + `wide` + `memmap2` | f16 quantized vectors, portable SIMD, memory-mapped I/O |
| `ftui` + `ftui-extras` | FrankenTUI terminal interface (path dep) |
| `toon` | Terminal rendering library (path dep) |
| `reqwest` | HTTP client (rustls-tls, blocking + async) |
| `rayon` | Data parallelism for CPU-bound work |
| `colored` + `indicatif` + `console` | Colorful, informative console output |
| `notify` | Filesystem watching |
| `walkdir` + `glob` | Directory traversal and pattern matching |
| `blake3` + `sha2` | Cryptographic hashing |
| `aes-gcm` + `ring` + `pbkdf2` + `argon2` | Encryption (ChatGPT conversations, HTML export) |
| `ssh2` | SFTP fallback for multi-machine sync |
| `dialoguer` | Interactive terminal prompts (setup wizard) |
| `syntect` | Syntax highlighting |
| `thiserror` | Ergonomic error type derivation |
| `tracing` | Structured logging and diagnostics |
| `unicode-normalization` | NFC text canonicalization |

**Path dependencies** (sibling dirs under `/data/projects/`):
- `frankensqlite` — Pure-Rust SQLite with BEGIN CONCURRENT (MVCC multi-writer)
- `frankensearch` — Unified search: BM25 lexical + semantic embeddings + RRF fusion + reranking
- `franken_agent_detection` — Auto-discovers agent sessions from 15+ providers
- `frankentui` (`ftui` + `ftui-extras` + `ftui-runtime` + `ftui-tty`) — Terminal UI framework
- `asupersync` — Async runtime (multi-thread, fs, spawn_blocking, signals)
- `toon` — Token-optimized serialization

### Release Profile

The release build optimizes for binary size (this is a CLI tool):

```toml
[profile.release]
lto = true          # Link-time optimization
codegen-units = 1   # Single codegen unit for better optimization
strip = true        # Remove debug symbols
panic = "abort"     # Abort on panic (smaller binary)
opt-level = "z"     # Optimize for size
```

A profiling profile is also available:

```toml
[profile.profiling]
inherits = "release"
debug = true        # Keep debug symbols for flamegraphs
strip = false
```

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Brittle regex-based transformations create far more problems than they solve.

- **Always make code changes manually**, even when there are many instances
- For many simple changes: use parallel subagents
- For subtle/complex changes: do them methodically yourself

### No File Proliferation

If you want to change something or add a feature, **revise existing code files in place**.

**NEVER** create variations like:
- `document_processorV2.rs`
- `document_processor_improved.rs`
- `document_processor_enhanced.rs`

New files are reserved for **genuinely new functionality** that makes zero sense to include in any existing file. The bar for creating new files is **incredibly high**.

---

## Backwards Compatibility

We do not care about backwards compatibility—we're in early development with no users. We want to do things the **RIGHT** way with **NO TECH DEBT**.

- Never create "compatibility shims"
- Never create wrapper functions for deprecated APIs
- Just fix the code directly

---

## Console Output Style

All console output should be **informative, detailed, stylish, and colorful** by leveraging:
- `colored` — ANSI color formatting
- `indicatif` — Progress bars and spinners
- `console` — Terminal utilities

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings
cargo check --all-targets

# Check for clippy lints
cargo clippy --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

---

## Database Guidelines (frankensqlite + rusqlite)

The project is migrating from rusqlite to frankensqlite. Both are available:
- `frankensqlite` (import as `fsqlite`) — Pure-Rust SQLite with BEGIN CONCURRENT support
- `rusqlite` — C-binding SQLite, retained as fallback during migration

### frankensqlite Patterns

```rust
use frankensqlite::Connection;

// Open with WAL mode (REQUIRED for concurrent access)
let conn = Connection::open(path)?;
conn.execute("PRAGMA journal_mode = WAL;")?;
conn.execute("PRAGMA busy_timeout = 5000;")?;

// Use params! macro (needs explicit import)
use fsqlite::params;
conn.execute_with_params("INSERT INTO t (a) VALUES (?1)", params![42])?;
```

### Verified Standard SQLite File Reads

`frankensqlite::Connection::open()` can open and read standard SQLite database files created by SQLite/rusqlite. That includes external app databases such as Cursor `state.vscdb`, OpenCode `opencode.db`, and historical cass databases.

- Do not add `rusqlite` just to read an existing SQLite file.
- If a specific query shape fails against one of these files, treat it as a targeted engine/query bug and file a reproducer instead of assuming the file format is unsupported.

### FrankenConnectionManager (production pattern)

Use `FrankenConnectionManager` for concurrent access:
- Reader pool (multiple concurrent readers)
- Writer token (single writer at a time via `WriterGuard`)
- `WriterGuard` auto-rollbacks on drop (RAII safety)

### Concurrent Writer Best Practices

1. **Always use WAL mode** — without it, concurrent writes corrupt the DB
2. **Use jittered exponential backoff** on `BusySnapshot` / `WriteConflict` errors
3. **Batch writes** — 10-20 rows per transaction (not 1 row per commit)
4. **Limit concurrent writers** to 4 threads (matches production rayon parallelism)
5. **Retryable errors:** `Busy`, `BusyRecovery`, `BusySnapshot`, `WriteConflict`, `SerializationFailure`, `DatabaseCorrupt`

### Known frankensqlite Differences

- **File format incompatibility:** frankensqlite databases cannot be read by C
  SQLite (rusqlite) and vice versa. Historical bundle salvage still uses rusqlite
  for reading pre-migration databases.
- **`PRAGMA writable_schema`:** Not supported for write operations (INSERT/UPDATE
  on sqlite_master). SELECT from sqlite_master works.

### General Rules

**Do:**
- Create connection pools and reuse across the application
- Use `?` placeholders for parameters (prevents SQL injection)
- Keep one database transaction per logical operation
- Handle migrations properly
- Use strong typing for database columns

**Don't:**
- Share a single transaction across concurrent tasks
- Use string concatenation to build SQL queries
- Ignore `Option<T>` for nullable columns
- Mix sync and async database operations
- Use `unwrap()` on database results in production code

---

## E2E Browser Tests

**IMPORTANT:** E2E browser tests (Playwright) should only be run on GitHub Actions CI, NOT locally.

Running browser tests locally:
- Consumes significant system resources (spawns browser instances)
- Can freeze or slow down the development machine
- May have different results than CI due to environment differences

**Push to a branch and let GitHub Actions run the tests.** The CI workflow in `.github/workflows/browser-tests.yml` handles:
- Installing browsers
- Running tests in parallel across Chromium, Firefox, and WebKit
- Uploading test artifacts and reports

If you need to debug a specific test, use `test.only()` and run a single spec file, but prefer CI for full test runs.

---

## Testing

### Testing Policy

Tests must cover:
- Happy path
- Edge cases (empty input, max values, boundary conditions)
- Error conditions

Integration and E2E tests live in the `tests/` directory. Benchmarks live in `benches/`.

### Unit Tests

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run a specific test
cargo test test_name

# Run tests with all features enabled
cargo test --all-features
```

### Test Categories

| Directory / File | Focus Areas |
|-----------------|-------------|
| `tests/connector_*.rs` | Per-provider session parsing (Claude, Codex, Cursor, Gemini, Aider, Amp, Cline, OpenCode, Pi Agent, Copilot, OpenClaw, ClawdBot, Vibe) |
| `tests/search_*.rs` | Search pipeline, caching, filters, wildcard fallback |
| `tests/semantic_integration.rs` | Semantic search, embeddings, two-tier search |
| `tests/e2e_*.rs` | End-to-end CLI flows, filters, search, sources, TUI, deploy |
| `tests/cli_*.rs` | CLI dispatch coverage, robot mode, index, stats |
| `tests/tui_*.rs` | TUI headless smoke tests, snapshot tests |
| `tests/tui_integration_smoke.rs` | TUI + full integrated stack (frankensqlite + frankensearch + FAD) |
| `tests/frankensqlite_*.rs` | frankensqlite compat gates, concurrent stress tests |
| `tests/agent_detection_completeness.rs` | franken_agent_detection connector completeness |
| `tests/html_export*.rs` | HTML export pipeline, encryption |
| `tests/storage*.rs` | SQLite storage, migration safety |
| `tests/performance/` | Performance regression tests |
| `benches/` | Criterion benchmarks (index, runtime, search, crypto, db, export, cache, regex, integration_regression) |

### Test Fixtures

Fixtures are in `tests/fixtures/` and cover multiple agent session formats for cross-connector validation.

---

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and current best practices.

---

## cass — Coding Agent Session Search

**This is the project you're working on.** cass indexes conversations from Claude Code, Codex, Cursor, Gemini, Aider, Amp, Cline, OpenCode, Pi Agent, Copilot, OpenClaw, ClawdBot, Vibe, and more into a unified, searchable index with a TUI and robot-mode CLI.

**NEVER run bare `cass`** — it launches an interactive TUI. Always use `--robot` or `--json`.

### What It Does

Provides unified full-text and semantic search across all local coding agent session histories, with a rich TUI, robot-mode JSON API, multi-machine sync, HTML export with optional encryption, and analytics.

### Project Structure

```
coding_agent_session_search/
├── Cargo.toml                    # Single-crate project
├── src/
│   ├── main.rs                   # Entry point (binary: cass)
│   ├── lib.rs                    # Library root
│   ├── connectors/               # Per-agent session parsers
│   │   ├── mod.rs                # Connector trait + registry
│   │   ├── claude_code.rs        # Claude Code sessions
│   │   ├── codex.rs              # Codex sessions
│   │   ├── cursor.rs             # Cursor sessions
│   │   ├── gemini.rs             # Gemini sessions
│   │   ├── aider.rs              # Aider sessions
│   │   ├── amp.rs                # Amp sessions
│   │   ├── chatgpt.rs            # ChatGPT sessions (encrypted)
│   │   ├── cline.rs              # Cline sessions
│   │   ├── opencode.rs           # OpenCode sessions
│   │   ├── pi_agent.rs           # Pi Agent sessions
│   │   ├── copilot.rs            # Copilot sessions
│   │   ├── openclaw.rs           # OpenClaw sessions
│   │   ├── clawdbot.rs           # ClawdBot sessions
│   │   ├── vibe.rs               # Vibe sessions
│   │   └── factory.rs            # Connector factory
│   ├── search/                   # Search engine (delegates to frankensearch)
│   │   ├── query.rs              # Query parsing and execution
│   │   ├── tantivy.rs            # BM25 full-text search (via frankensearch)
│   │   ├── vector_index.rs       # Vector similarity search
│   │   ├── two_tier_search.rs    # Progressive 2-tier hybrid search
│   │   ├── ann_index.rs          # HNSW approximate nearest neighbors
│   │   ├── hash_embedder.rs      # FNV-1a hash embedder (fast, zero-dep)
│   │   ├── fastembed_embedder.rs # ONNX-based quality embedder
│   │   ├── embedder.rs           # Embedder trait
│   │   ├── embedder_registry.rs  # Embedder auto-detection
│   │   ├── reranker.rs           # Cross-encoder reranking
│   │   ├── reranker_registry.rs  # Reranker management
│   │   ├── model_download.rs     # Model download management
│   │   ├── model_manager.rs      # Model lifecycle management
│   │   ├── canonicalize.rs       # Query canonicalization
│   │   └── daemon_client.rs      # Search daemon RPC client
│   ├── indexer/                  # Session indexing pipeline
│   ├── storage/                  # SQLite persistence (frankensqlite + rusqlite)
│   ├── ui/                       # TUI components
│   ├── pages/                    # Web pages generation
│   ├── pages_assets/             # Static assets for pages
│   ├── html_export/              # Self-contained HTML export
│   ├── analytics/                # Usage analytics
│   ├── daemon/                   # Background search daemon
│   ├── sources/                  # Multi-machine source management
│   ├── model/                    # Data models
│   ├── bookmarks.rs              # Session bookmarking
│   ├── bakeoff.rs                # Embedder comparison tool
│   ├── encryption.rs             # AES-GCM encryption
│   ├── export.rs                 # Export pipeline
│   ├── update_check.rs           # Auto-update checking
│   └── tui_asciicast.rs          # Terminal recording
├── tests/                        # Integration + E2E tests
├── benches/                      # Criterion benchmarks
├── scripts/                      # Helper scripts
├── web/                          # Web assets
├── docs/                         # Documentation
└── fuzz/                         # Fuzz testing
```

### Quick Start

```bash
# Check if index is healthy (exit 0=ok, 1=run index first)
cass health

# Search across all agent histories
cass search "authentication error" --robot --limit 5

# View a specific result (from search output)
cass view /path/to/session.jsonl -n 42 --json

# Expand context around a line
cass expand /path/to/session.jsonl -n 42 -C 3 --json

# Export session as self-contained HTML
cass export-html /path/to/session.jsonl --json
cass export-html session.jsonl --encrypt --password "secret" --json

# Learn the full API
cass capabilities --json      # Feature discovery
cass robot-docs guide         # LLM-optimized docs
```

### Supported Providers

| Provider | Connector | Session Format |
|----------|-----------|----------------|
| Claude Code | `claude_code.rs` | JSONL |
| Codex | `codex.rs` | JSONL |
| Cursor | `cursor.rs` | JSONL / SQLite |
| Gemini | `gemini.rs` | JSONL |
| Aider | `aider.rs` | Markdown / JSONL |
| Amp | `amp.rs` | JSONL |
| ChatGPT | `chatgpt.rs` | Encrypted JSON |
| Cline | `cline.rs` | JSONL |
| OpenCode | `opencode.rs` | JSONL |
| Pi Agent | `pi_agent.rs` | JSONL |
| Copilot | `copilot.rs` | JSONL |
| OpenClaw | `openclaw.rs` | JSONL |
| ClawdBot | `clawdbot.rs` | JSONL |
| Vibe | `vibe.rs` | JSONL |

### HTML Export (Robot Mode)

Export conversations as self-contained HTML files with optional encryption:

```bash
# Basic export (outputs to Downloads folder)
cass export-html /path/to/session.jsonl --json

# With encryption
cass export-html session.jsonl --encrypt --password "secret" --json

# Password from stdin (secure)
echo "secret" | cass export-html session.jsonl --encrypt --password-stdin --json

# Custom output
cass export-html session.jsonl --output-dir /tmp --filename "export" --json
```

**Robot mode JSON output:**
```json
{
  "success": true,
  "output_path": "/home/user/Downloads/claude_2026-01-25_session.html",
  "file_size": 145623,
  "encrypted": false,
  "message_count": 42
}
```

**Error codes:**
| Code | Kind | Description |
|------|------|-------------|
| 3 | session_not_found | Session file doesn't exist |
| 4 | output_not_writable | Cannot write to output directory |
| 5 | encryption_error | Encryption failed |
| 6 | password_required | --encrypt used without password |

### Key Flags

| Flag | Purpose |
|------|---------|
| `--robot` / `--json` | Machine-readable JSON output (required!) |
| `--fields minimal` | Reduce payload: `source_path`, `line_number`, `agent` only |
| `--limit N` | Cap result count |
| `--agent NAME` | Filter to specific agent (claude, codex, cursor, etc.) |
| `--days N` | Limit to recent N days |

**stdout = data only, stderr = diagnostics. Exit 0 = success.**

### Robot Mode Etiquette

- Prefer `cass --robot-help` and `cass robot-docs <topic>` for machine-first docs
- The CLI is forgiving: globals placed before/after subcommand are auto-normalized
- If parsing fails, follow the actionable errors with examples
- Use `--color=never` in non-TTY automation for ANSI-free output

### Auto-Correction Features

| Mistake | Correction | Note |
|---------|------------|------|
| `-robot` | `--robot` | Long flags need double-dash |
| `--Robot`, `--LIMIT` | `--robot`, `--limit` | Flags are lowercase |
| `find "query"` | `search "query"` | `find` is an alias |
| `--robot-docs` | `robot-docs` | It's a subcommand |

**Full alias list:**
- **Search:** `find`, `query`, `q`, `lookup`, `grep` -> `search`
- **Stats:** `ls`, `list`, `info`, `summary` -> `stats`
- **Status:** `st`, `state` -> `status`
- **Index:** `reindex`, `idx`, `rebuild` -> `index`
- **View:** `show`, `get`, `read` -> `view`
- **Robot-docs:** `docs`, `help-robot`, `robotdocs` -> `robot-docs`

### Pre-Flight Health Check

```bash
cass health --json
```

Returns in <50ms:
- **Exit 0:** Healthy — proceed with queries
- **Exit 1:** Unhealthy — run `cass index --full` first

### Exit Codes

| Code | Meaning | Retryable |
|------|---------|-----------|
| 0 | Success | N/A |
| 1 | Health check failed | Yes — run `cass index --full` |
| 2 | Usage/parsing error | No — fix syntax |
| 3 | Index/DB missing | Yes — run `cass index --full` |
| 4 | Network error | Yes — check connectivity |
| 5 | Data corruption | Yes — run `cass index --full --force-rebuild` |
| 6 | Incompatible version | No — update cass |
| 7 | Lock/busy | Yes — retry later |
| 8 | Partial result | Yes — increase timeout |
| 9 | Unknown error | Maybe |

### Multi-Machine Search Setup

cass can search across agent sessions from multiple machines. Use the interactive setup wizard for the easiest configuration:

```bash
cass sources setup
```

#### What the wizard does:
1. **Discovers** SSH hosts from your ~/.ssh/config
2. **Probes** each host to check for:
   - Existing cass installation (and version)
   - Agent session data (Claude, Codex, Cursor, Gemini)
   - System resources (disk, memory)
3. **Lets you select** which hosts to configure
4. **Installs cass** on remotes if needed
5. **Indexes** existing sessions on remotes
6. **Configures** sources.toml with correct paths
7. **Syncs** data to your local machine

#### For scripting (non-interactive):
```bash
cass sources setup --non-interactive --hosts css,csd,yto
cass sources setup --json --hosts css  # JSON output for parsing
```

#### Key flags:
| Flag | Purpose |
|------|---------|
| `--hosts <names>` | Configure only these hosts (comma-separated) |
| `--dry-run` | Preview without making changes |
| `--resume` | Resume interrupted setup |
| `--skip-install` | Don't install cass on remotes |
| `--skip-index` | Don't run remote indexing |
| `--skip-sync` | Don't sync after setup |
| `--json` | Output progress as JSON |

#### After setup:
```bash
# Search across all sources
cass search "database migration"

# Sync latest data
cass sources sync --all

# List configured sources
cass sources list
```

#### Manual configuration:
If you prefer manual setup, edit `~/.config/cass/sources.toml`:
```toml
[[sources]]
name = "my-server"
type = "ssh"
host = "user@server.example.com"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user/projects"
to = "/Users/me/projects"
```

#### Troubleshooting:
- **Host unreachable**: Verify SSH config with `ssh <host>` manually
- **Permission denied**: Load SSH key with `ssh-add ~/.ssh/id_rsa`
- **cargo not found**: Use `--skip-install` and install manually
- **Interrupted setup**: Resume with `cass sources setup --resume`

For machine-readable docs: `cass robot-docs sources`

### Feature Flags

```toml
[features]
default = ["qr", "encryption"]
qr = ["dep:qrcode", "dep:image"]         # QR code generation for recovery secret
encryption = []                            # HTML export encryption (deps included for ChatGPT)
backtrace = []                             # Enhanced backtraces
```

---

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources. Provides identities, inbox/outbox, searchable threads, and advisory file reservations with human-auditable artifacts in Git.

### Why It's Useful

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs
- **Token-efficient:** Messages stored in per-project archive, not in context
- **Quick reads:** `resource://inbox/...`, `resource://thread/...`

### Same Repository Workflow

1. **Register identity:**
   ```
   ensure_project(project_key=<abs-path>)
   register_agent(project_key, program, model)
   ```

2. **Reserve files before editing:**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
   ```

3. **Communicate with threads:**
   ```
   send_message(..., thread_id="FEAT-123")
   fetch_inbox(project_key, agent_name)
   acknowledge_message(project_key, agent_name, message_id)
   ```

4. **Quick reads:**
   ```
   resource://inbox/{Agent}?project=<abs-path>&limit=20
   resource://thread/{id}?project=<abs-path>&include_bodies=true
   ```

### Macros vs Granular Tools

- **Prefer macros for speed:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools for control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

### Common Pitfalls

- `"from_agent not registered"`: Always `register_agent` in the correct `project_key` first
- `"FILE_RESERVATION_CONFLICT"`: Adjust patterns, wait for expiry, or use non-exclusive reservation
- **Auth errors:** If JWT+JWKS enabled, include bearer token with matching `kid`

---

## Beads (br) — Dependency-Aware Issue Tracking

Beads provides a lightweight, dependency-aware issue database and CLI (`br` - beads_rust) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

**Important:** `br` is non-invasive—it NEVER runs git commands automatically. You must manually commit changes after `br sync --flush-only`.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `br-123`) as Mail `thread_id` and prefix subjects with `[br-123]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close 123 --reason "Completed"
   br sync --flush-only  # Export to JSONL (no git operations)
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[br-123] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `br-###` |
| Mail subject | `[br-###] ...` |
| File reservation `reason` | `br-###` |
| Commit messages | Include `br-###` for traceability |

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use ONLY `--robot-*` flags. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** It returns:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command
```

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |
| `--robot-label-health` | Per-label health: `health_level`, `velocity_score`, `staleness`, `blocked_count` |
| `--robot-label-flow` | Cross-label dependency: `flow_matrix`, `dependencies`, `bottleneck_labels` |
| `--robot-label-attention [--attention-limit=N]` | Attention-ranked labels |

**History & Change Tracking:**
| Command | Returns |
|---------|---------|
| `--robot-history` | Bead-to-commit correlations |
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues, cycles |

**Other:**
| Command | Returns |
|---------|---------|
| `--robot-burndown <sprint>` | Sprint burndown, scope changes, at-risk items |
| `--robot-forecast <id\|all>` | ETA predictions with dependency-aware scheduling |
| `--robot-alerts` | Stale issues, blocking cascades, priority mismatches |
| `--robot-suggest` | Hygiene: duplicates, missing deps, label suggestions |
| `--robot-graph [--graph-format=json\|dot\|mermaid]` | Dependency graph export |
| `--export-graph <file.html>` | Interactive HTML visualization |

### Scoping & Filtering

```bash
bv --robot-plan --label backend              # Scope to label's subgraph
bv --robot-insights --as-of HEAD~30          # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work
bv --recipe high-impact --robot-triage       # Pre-filter: top PageRank
bv --robot-triage --robot-triage-by-track    # Group by parallel work streams
bv --robot-triage --robot-triage-by-label    # Group by domain
```

### Understanding Robot Output

**All robot JSON includes:**
- `data_hash` — Fingerprint of source beads.jsonl
- `status` — Per-metric state: `computed|approx|timeout|skipped` + elapsed ms
- `as_of` / `as_of_commit` — Present when using `--as-of`

**Two-phase analysis:**
- **Phase 1 (instant):** degree, topo sort, density
- **Phase 2 (async, 500ms timeout):** PageRank, betweenness, HITS, eigenvector, cycles

### jq Quick Reference

```bash
bv --robot-triage | jq '.quick_ref'                        # At-a-glance summary
bv --robot-triage | jq '.recommendations[0]'               # Top recommendation
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-insights | jq '.status'                         # Check metric readiness
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```

---

## UBS — Ultimate Bug Scanner

**Golden Rule:** `ubs <changed-files>` before every commit. Exit 0 = safe. Exit >0 = fix & re-run.

### Commands

```bash
ubs file.rs file2.rs                    # Specific files (< 1s) — USE THIS
ubs $(git diff --name-only --cached)    # Staged files — before commit
ubs --only=rust,toml src/               # Language filter (3-5x faster)
ubs --ci --fail-on-warning .            # CI mode — before PR
ubs .                                   # Whole project (ignores target/, Cargo.lock)
```

### Output Format

```
Warning  Category (N errors)
    file.rs:42:5 - Issue description
    Suggested fix
Exit code: 1
```

Parse: `file:line:col` -> location | Suggested fix -> how to fix | Exit 0/1 -> pass/fail

### Fix Workflow

1. Read finding -> category + fix suggestion
2. Navigate `file:line:col` -> view context
3. Verify real issue (not false positive)
4. Fix root cause (not symptom)
5. Re-run `ubs <file>` -> exit 0
6. Commit

### Bug Severity

- **Critical (always fix):** Memory safety, use-after-free, data races, SQL injection
- **Important (production):** Unwrap panics, resource leaks, overflow checks
- **Contextual (judgment):** TODO/FIXME, println! debugging

---

## RCH — Remote Compilation Helper

RCH offloads `cargo build`, `cargo test`, `cargo clippy`, and other compilation commands to a fleet of 8 remote Contabo VPS workers instead of building locally. This prevents compilation storms from overwhelming csd when many agents run simultaneously.

**RCH is installed at `~/.local/bin/rch` and is hooked into Claude Code's PreToolUse automatically.** Most of the time you don't need to do anything if you are Claude Code — builds are intercepted and offloaded transparently.

To manually offload a build:
```bash
rch exec -- cargo build --release
rch exec -- cargo test
rch exec -- cargo clippy
```

Quick commands:
```bash
rch doctor                    # Health check
rch workers probe --all       # Test connectivity to all 8 workers
rch status                    # Overview of current state
rch queue                     # See active/waiting builds
```

If rch or its workers are unavailable, it fails open — builds run locally as normal.

**Note for Codex/GPT-5.2:** Codex does not have the automatic PreToolUse hook, but you can (and should) still manually offload compute-intensive compilation commands using `rch exec -- <command>`. This avoids local resource contention when multiple agents are building simultaneously.

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** -> `ast-grep`
- Need raw speed or **hunting text** -> `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$$ARGS) -> $RET { $$$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

---

## Morph Warp Grep — AI-Powered Code Search

**Use `mcp__morph-mcp__warp_grep` for exploratory "how does X work?" questions.** An AI agent expands your query, greps the codebase, reads relevant files, and returns precise line ranges with full context.

**Use `ripgrep` for targeted searches.** When you know exactly what you're looking for.

**Use `ast-grep` for structural patterns.** When you need AST precision for matching/rewriting.

### When to Use What

| Scenario | Tool | Why |
|----------|------|-----|
| "How is authentication implemented?" | `warp_grep` | Exploratory; don't know where to start |
| "Where is rate limiting implemented?" | `warp_grep` | Need to understand architecture |
| "Find all uses of `embed(`" | `ripgrep` | Targeted literal search |
| "Find files with `println!`" | `ripgrep` | Simple pattern |
| "Replace all `unwrap()` with `expect()`" | `ast-grep` | Structural refactor |

### warp_grep Usage

```
mcp__morph-mcp__warp_grep(
  repoPath: "/dp/coding_agent_session_search",
  query: "How is semantic search implemented?"
)
```

Returns structured results with file paths, line ranges, and extracted code snippets.

### Anti-Patterns

- **Don't** use `warp_grep` to find a specific function name -> use `ripgrep`
- **Don't** use `ripgrep` to understand "how does X work" -> wastes time with manual reads
- **Don't** use `ripgrep` for codemods -> risks collateral edits

<!-- bv-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

**Important:** `br` is non-invasive—it NEVER executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
br ready              # Show issues ready to work (no blockers)
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason "Completed"
br close <id1> <id2>  # Close multiple issues at once
br sync --flush-only  # Export to JSONL (NO git operations)
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Run `br sync --flush-only` then manually commit

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads to JSONL
git add .beads/         # Stage beads changes
git commit -m "..."     # Commit everything together
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress -> closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always `br sync --flush-only && git add .beads/` before ending session

<!-- end-bv-agent-instructions -->

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Sync beads** - `br sync --flush-only` to export to JSONL
5. **Hand off** - Provide context for next session

---

## cass — Cross-Agent Session Search

`cass` indexes prior agent conversations (Claude Code, Codex, Cursor, Gemini, ChatGPT, etc.) so we can reuse solved problems.

**Rules:** Never run bare `cass` (TUI). Always use `--robot` or `--json`.

### Examples

```bash
cass health
cass search "async runtime" --robot --limit 5
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json
cass capabilities --json
cass robot-docs guide
```

### Tips

- Use `--fields minimal` for lean output
- Filter by agent with `--agent`
- Use `--days N` to limit to recent history

stdout is data-only, stderr is diagnostics; exit code 0 means success.

Treat cass as a way to avoid re-solving problems other agents already handled.

---

Note for Codex/GPT-5.2:

You constantly bother me and stop working with concerned questions that look similar to this:

```
Unexpected changes (need guidance)

- Working tree still shows edits I did not make in Cargo.toml, Cargo.lock, src/main.rs, src/patterns.rs. Please advise whether to keep/commit/revert these before any further work. I did not touch them.

Next steps (pick one)

1. Decide how to handle the unrelated modified files above so we can resume cleanly.
```

NEVER EVER DO THAT AGAIN. The answer is literally ALWAYS the same: those are changes created by the potentially dozen of other agents working on the project at the same time. This is not only a common occurrence, it happens multiple times PER MINUTE. The way to deal with it is simple: you NEVER, under ANY CIRCUMSTANCE, stash, revert, overwrite, or otherwise disturb in ANY way the work of other agents. Just treat those changes identically to changes that you yourself made. Just fool yourself into thinking YOU made the changes and simply don't recall it for some reason.

---

## Note on Built-in TODO Functionality

Also, if I ask you to explicitly use your built-in TODO functionality, don't complain about this and say you need to use beads. You can use built-in TODOs if I tell you specifically to do so. Always comply with such orders.
