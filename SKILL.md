---
name: cass
description: "Coding Agent Session Search - unified CLI/TUI to index and search local coding agent history from Claude Code, Codex, Gemini, Cursor, Aider, ChatGPT, Pi-Agent, Factory, and more. Purpose-built for AI agent consumption with robot mode."
---

# CASS - Coding Agent Session Search

Unified, high-performance CLI/TUI to index and search your local coding agent history. Aggregates sessions from **23 agents**, including Codex, Claude Code, Gemini CLI, Cline, OpenCode, Amp, Cursor, ChatGPT, Aider, Pi-Agent, Factory (Droid), OpenHands, Antigravity, and Grok Build.

## CRITICAL: Robot Mode Required for AI Agents

**NEVER run bare `cass`** - it launches an interactive TUI that blocks your session!

```bash
# WRONG - blocks terminal
cass

# CORRECT - JSON output for agents
cass search "query" --robot
cass search "query" --json  # alias
```

**Always use `--robot` or `--json` flags for machine-readable output.**

---

## Quick Reference for AI Agents

### Pre-Flight Check

```bash
# Health check (exit 0=healthy, 1=unhealthy, <50ms)
cass health

# If unhealthy, rebuild index
cass index --full
```

### Essential Commands

```bash
# Find the current session for this workspace
cass sessions --current --json

# List recent sessions for a specific project
cass sessions --workspace "$(pwd)" --json --limit 5

# Common agent flow: find current session, then export it
cass export-html "$(cass sessions --current --json | jq -r '.sessions[0].path')" --json

# Search with JSON output
cass search "authentication error" --robot --limit 5

# Search with metadata (elapsed_ms, cache stats, freshness)
cass search "error" --robot --robot-meta

# Minimal payload (path, line, agent only)
cass search "bug" --robot --fields minimal

# View source at specific line
cass view /path/to/session.jsonl -n 42 --json

# Expand context around a line
cass expand /path/to/session.jsonl -n 42 -C 5 --json

# Capabilities discovery
cass capabilities --json

# Full API schema
cass introspect --json

# LLM-optimized documentation
cass robot-docs guide
cass robot-docs commands
cass robot-docs schemas
cass robot-docs examples
cass robot-docs exit-codes
```

---

## Why Use CASS

### Cross-Agent Knowledge Transfer

Your coding agents create scattered knowledge:
- Claude Code sessions in `~/.claude/projects`
- Codex sessions in `~/.codex/sessions`
- Cursor state in SQLite databases
- Aider history in markdown files

CASS **unifies all of this** into a single searchable index. When you're stuck on a problem, search across ALL your past agent sessions to find relevant solutions.

### Use Cases

```bash
# "I solved this before..."
cass search "TypeError: Cannot read property" --robot --days 30

# Cross-agent learning (what has ANY agent said about X?)
cass search "authentication" --robot --workspace /path/to/project

# Agent-to-agent handoff
cass search "database migration" --robot --fields summary

# Daily review
cass timeline --today --json
```

---

## Command Reference

### Indexing

```bash
# Full rebuild of DB and search index
cass index --full

# Incremental update (since last scan)
cass index

# Watch mode: auto-reindex on file changes
cass index --watch

# Force rebuild even if schema unchanged
cass index --full --force-rebuild

# Safe retries with idempotency key (24h TTL)
cass index --full --idempotency-key "build-$(date +%Y%m%d)"

# JSON output with stats
cass index --full --json
```

### Search

```bash
# Basic search (JSON output required for agents!)
cass search "query" --robot

# With filters
cass search "error" --robot --agent claude --days 7
cass search "bug" --robot --workspace /path/to/project
cass search "panic" --robot --today

# Time filters
cass search "auth" --robot --since 2024-01-01 --until 2024-01-31
cass search "test" --robot --yesterday
cass search "fix" --robot --week

# Wildcards
cass search "auth*" --robot          # prefix: authentication, authorize
cass search "*tion" --robot          # suffix: authentication, exception
cass search "*config*" --robot       # substring: misconfigured

# Token budget management (critical for LLMs!)
cass search "error" --robot --fields minimal              # path, line, agent only
cass search "error" --robot --fields summary              # adds title, score
cass search "error" --robot --max-content-length 500      # truncate fields
cass search "error" --robot --max-tokens 2000             # soft budget (~4 chars/token)
cass search "error" --robot --limit 5                     # cap results

# Pagination (cursor-based)
cass search "TODO" --robot --robot-meta --limit 20
# Use _meta.next_cursor from response:
cass search "TODO" --robot --robot-meta --limit 20 --cursor "eyJ..."

# Match highlighting
cass search "authentication error" --robot --highlight

# Query analysis/debugging
cass search "auth*" --robot --explain    # parsed query, cost estimates
cass search "auth error" --robot --dry-run  # validate without executing

# Aggregations (server-side counts)
cass search "error" --robot --aggregate agent,workspace,date

# Request correlation
cass search "bug" --robot --request-id "req-12345"

# Source filtering (for multi-machine setups)
cass search "auth" --robot --source laptop
cass search "error" --robot --source remote

# Traceability (for debugging agent pipelines)
cass search "error" --robot --trace-file /tmp/cass-trace.json
```

### Session Analysis

```bash
# Export conversation to markdown/HTML/JSON
cass export /path/to/session.jsonl --format markdown -o conversation.md
cass export /path/to/session.jsonl --format html -o conversation.html
cass export /path/to/session.jsonl --format json --include-tools

# Expand context around a line (from search result)
cass expand /path/to/session.jsonl -n 42 -C 5 --json
# Shows 5 messages before and after line 42

# View source at line
cass view /path/to/session.jsonl -n 42 --json

# Activity timeline
cass timeline --today --json --group-by hour
cass timeline --days 7 --json --agent claude
cass timeline --since 7d --json

# Find related sessions for a file
cass context /path/to/source.ts --json
```

### Status & Diagnostics

```bash
# Quick health (<50ms)
cass health
cass health --json

# Full status snapshot
cass status --json
cass state --json  # alias

# Statistics
cass stats --json
cass stats --by-source  # for multi-machine

# Full diagnostics
cass diag --verbose
```

---

## Aggregation & Analytics

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

| Aggregation Field | Description |
|-------------------|-------------|
| `agent` | Group by agent type (claude_code, codex, cursor, etc.) |
| `workspace` | Group by workspace/project path |
| `date` | Group by date (YYYY-MM-DD) |
| `match_type` | Group by match quality (exact, prefix, fuzzy) |

Top 10 buckets returned per field, with `other_count` for remaining items.

---

## Remote Sources (Multi-Machine Search)

Search across sessions from multiple machines via SSH/rsync.

### Setup Wizard (Recommended)

```bash
cass sources setup
```

The wizard:
1. Discovers SSH hosts from `~/.ssh/config`
2. Probes each for agent data and cass installation
3. Optionally installs cass on remotes
4. Indexes sessions on remotes
5. Configures `sources.toml`
6. Syncs data locally

```bash
cass sources setup --hosts css,csd,yto  # Specific hosts only
cass sources setup --dry-run             # Preview without changes
cass sources setup --resume              # Resume interrupted setup
```

### Manual Setup

```bash
# Add a remote machine
cass sources add user@laptop.local --preset macos-defaults
cass sources add dev@workstation --path ~/.claude/projects --path ~/.codex/sessions

# List sources
cass sources list --json

# Sync sessions
cass sources sync
cass sources sync --source laptop --verbose

# Check connectivity
cass sources doctor
cass sources doctor --source laptop --json

# Path mappings (rewrite remote paths to local)
cass sources mappings list laptop
cass sources mappings add laptop --from /home/user/projects --to /Users/me/projects
cass sources mappings test laptop /home/user/projects/myapp/src/main.rs

# Remove source
cass sources remove laptop --purge -y
```

Configuration stored in `~/.config/cass/sources.toml` (Linux) or `~/Library/Application Support/cass/sources.toml` (macOS).

---

## Robot Mode Deep Dive

### Self-Documenting API

CASS teaches agents how to use itself:

```bash
# Quick capability check
cass capabilities --json
# Returns: features, connectors, limits

# Full API schema
cass introspect --json
# Returns: all commands, arguments, response shapes

# Topic-based docs (LLM-optimized)
cass robot-docs commands   # all commands and flags
cass robot-docs schemas    # response JSON schemas
cass robot-docs examples   # copy-paste invocations
cass robot-docs exit-codes # error handling
cass robot-docs guide      # quick-start walkthrough
cass robot-docs contracts  # API versioning
cass robot-docs sources    # remote sources guide
```

### Forgiving Syntax (Agent-Friendly)

CASS auto-corrects common mistakes:

| What you type | What CASS understands |
|---------------|----------------------|
| `cass serach "error"` | `cass search "error"` (typo corrected) |
| `cass -robot -limit=5` | `cass --robot --limit=5` (single-dash fixed) |
| `cass --Robot --LIMIT 5` | `cass --robot --limit 5` (case normalized) |
| `cass find "auth"` | `cass search "auth"` (alias resolved) |
| `cass --limt 5` | `cass --limit 5` (Levenshtein <=2) |

**Command Aliases:**
- `find`, `query`, `q`, `lookup`, `grep` → `search`
- `ls`, `list`, `info`, `summary` → `stats`
- `st`, `state` → `status`
- `reindex`, `idx`, `rebuild` → `index`
- `show`, `get`, `read` → `view`
- `docs`, `help-robot`, `robotdocs` → `robot-docs`

### Output Formats

```bash
# Pretty-printed JSON (default)
cass search "error" --robot

# Streaming JSONL (header + one hit per line)
cass search "error" --robot-format jsonl

# Compact single-line JSON
cass search "error" --robot-format compact

# With performance metadata
cass search "error" --robot --robot-meta
```

**Design principle:** stdout = JSON only; diagnostics go to stderr.

### Token Budget Management

LLMs have context limits. Control output size:

| Flag | Effect |
|------|--------|
| `--fields minimal` | Only `source_path`, `line_number`, `agent` |
| `--fields summary` | Adds `title`, `score` |
| `--fields score,title,snippet` | Custom field selection |
| `--max-content-length 500` | Truncate long fields (UTF-8 safe) |
| `--max-tokens 2000` | Soft budget (~4 chars/token) |
| `--limit 5` | Cap number of results |

Truncated fields include `*_truncated: true` indicator.

---

## Structured Error Handling

Errors are JSON with actionable hints:

```json
{
  "error": {
    "code": 3,
    "kind": "index_missing",
    "message": "Search index not found",
    "hint": "Run 'cass index --full' to build the index",
    "retryable": false
  }
}
```

### Exit Codes

| Code | Meaning | Action |
|------|---------|--------|
| 0 | Success | Parse stdout |
| 1 | Health check failed | Run `cass index --full` |
| 2 | Usage error | Fix syntax (hint provided) |
| 3 | Index/DB missing | Run `cass index --full` |
| 4 | Network error | Check connectivity |
| 5 | Data corruption | Run `cass index --full --force-rebuild` |
| 6 | Incompatible version | Update cass |
| 7 | Lock/busy | Retry later |
| 8 | Partial result | Increase `--timeout` |
| 9 | Unknown error | Check `retryable` flag |

---

## Search Modes

Three search modes, selectable with `--mode` flag:

| Mode | Algorithm | Best For |
|------|-----------|----------|
| **lexical** (default) | BM25 full-text | Exact term matching, code searches |
| **semantic** | Vector similarity | Conceptual queries, "find similar" |
| **hybrid** | Reciprocal Rank Fusion | Balanced precision and recall |

```bash
cass search "authentication" --mode lexical --robot
cass search "how to handle user login" --mode semantic --robot
cass search "auth error handling" --mode hybrid --robot
```

**Hybrid** combines lexical and semantic using RRF:
```
RRF_score = Σ 1 / (60 + rank_i)
```

---

## Pipeline Mode (Chained Search)

Chain searches by piping session paths:

```bash
# Find sessions mentioning "auth", then search within those for "token"
cass search "authentication" --robot-format sessions | \
  cass search "refresh token" --sessions-from - --robot

# Build a filtered corpus from today's work
cass search --today --robot-format sessions > today_sessions.txt
cass search "bug fix" --sessions-from today_sessions.txt --robot
```

Use cases:
- **Drill-down**: Broad search → narrow within results
- **Cross-reference**: Find sessions with term A, then find term B within them
- **Corpus building**: Save session lists for repeated searches

---

## Query Language

### Basic Queries

| Query | Matches |
|-------|---------|
| `error` | Messages containing "error" (case-insensitive) |
| `python error` | Both "python" AND "error" |
| `"authentication failed"` | Exact phrase |

### Boolean Operators

| Operator | Example | Meaning |
|----------|---------|---------|
| `AND` | `python AND error` | Both terms required (default) |
| `OR` | `error OR warning` | Either term matches |
| `NOT` | `error NOT test` | First term, excluding second |
| `-` | `error -test` | Shorthand for NOT |

```bash
# Complex boolean query
cass search "authentication AND (error OR failure) NOT test" --robot

# Exclude test files
cass search "bug fix -test -spec" --robot

# Either error type
cass search "TypeError OR ValueError" --robot
```

### Wildcard Patterns

| Pattern | Type | Performance |
|---------|------|-------------|
| `auth*` | Prefix | Fast (edge n-grams) |
| `*tion` | Suffix | Slower (regex) |
| `*config*` | Substring | Slowest (regex) |

### Match Types

Results include `match_type`:

| Type | Meaning | Score Boost |
|------|---------|-------------|
| `exact` | Verbatim match | Highest |
| `prefix` | Via prefix expansion | High |
| `suffix` | Via suffix pattern | Medium |
| `substring` | Via substring pattern | Lower |
| `fuzzy` | Auto-fallback (sparse results) | Lowest |

### Auto-Fuzzy Fallback

When exact query returns <3 results, CASS automatically retries with wildcards:
- `auth` → `*auth*`
- Results flagged with `wildcard_fallback: true`

### Flexible Time Input

CASS accepts a wide variety of time/date formats:

| Format | Examples |
|--------|----------|
| **Relative** | `-7d`, `-24h`, `-30m`, `-1w` |
| **Keywords** | `now`, `today`, `yesterday` |
| **ISO 8601** | `2024-11-25`, `2024-11-25T14:30:00Z` |
| **US Dates** | `11/25/2024`, `11-25-2024` |
| **Unix Timestamp** | `1732579200` (seconds or milliseconds) |

---

## Ranking Modes

Cycle with `F12` in TUI or use `--ranking` flag:

| Mode | Formula | Best For |
|------|---------|----------|
| **Recent Heavy** | `relevance*0.3 + recency*0.7` | "What was I working on?" |
| **Balanced** | `relevance*0.5 + recency*0.5` | General search |
| **Relevance** | `relevance*0.8 + recency*0.2` | "Best explanation of X" |
| **Match Quality** | Penalizes fuzzy matches | Precise technical searches |
| **Date Newest** | Pure chronological | Recent activity |
| **Date Oldest** | Reverse chronological | "When did I first..." |

### Score Components

- **Text Relevance (BM25)**: Term frequency, inverse document frequency, length normalization
- **Recency**: Exponential decay (today ~1.0, last week ~0.7, last month ~0.3)
- **Match Exactness**: Exact phrase=1.0, Prefix=0.9, Suffix=0.8, Substring=0.6, Fuzzy=0.4

### Blended Scoring Formula

```
Final_Score = BM25_Score × Match_Quality + α × Recency_Factor
```

| Mode | α Value | Effect |
|------|---------|--------|
| Recent Heavy | 1.0 | Recency dominates |
| Balanced | 0.4 | Moderate recency boost |
| Relevance Heavy | 0.1 | BM25 dominates |
| Match Quality | 0.0 | Pure text matching |

---

## Supported Agents (23 Connectors)

| Agent | Location | Format |
|-------|----------|--------|
| **Claude Code** | `~/.claude/projects` | JSONL |
| **Codex** | `~/.codex/sessions` | JSONL (Rollout) |
| **Gemini CLI** | `~/.gemini/tmp` | JSON |
| **Cline** | VS Code global storage | Task directories |
| **OpenCode** | `.opencode` directories | SQLite |
| **Amp** | `~/.local/share/amp` + VS Code | Mixed |
| **Cursor** | `~/Library/Application Support/Cursor` | SQLite (state.vscdb) |
| **ChatGPT** | `~/Library/Application Support/com.openai.chat` | JSON (v1 unencrypted) |
| **Aider** | `~/.aider.chat.history.md` + per-project | Markdown |
| **Pi-Agent** | `~/.pi/agent/sessions` | JSONL with thinking |
| **Factory (Droid)** | `~/.factory/sessions` | JSONL by workspace |
| **GitHub Copilot Chat** | VS Code global storage | JSON / SQLite |
| **GitHub Copilot CLI** | `~/.copilot/session-state` | JSONL |
| **OpenClaw** | `~/.openclaw` | JSONL |
| **ClawdBot** | `~/.clawdbot` | JSONL |
| **Vibe** | `~/.vibe/logs/session` | JSONL |
| **Crush** | `~/.crush/crush.db` | SQLite |
| **Hermes** | `~/.hermes` | JSONL |
| **Kimi Code** | `~/.kimi/sessions` | JSONL |
| **Qwen Code** | `~/.qwen/tmp` | JSON / JSONL |
| **OpenHands** | `~/.openhands/conversations` | JSON event stream |
| **Antigravity** | `~/.gemini/antigravity-cli` | JSONL / SQLite |
| **Grok Build** | `$GROK_HOME/sessions` (default `~/.grok/sessions`) | ACP updates JSONL |

**Note:** ChatGPT v2/v3 are AES-256-GCM encrypted (keychain access required). Legacy v1 unencrypted conversations are indexed automatically.

---

## TUI Features (for Humans)

Launch with `cass` (no flags):

### Keyboard Shortcuts

**Navigation:**
- `Up/Down`: Move selection
- `Left/Right`: Switch panes
- `Tab/Shift+Tab`: Cycle focus
- `Enter`: Open in `$EDITOR`
- `Space`: Full-screen detail view
- `Home/End`: Jump to first/last result
- `PageUp/PageDown`: Scroll by page

**Filtering:**
- `F3`: Agent filter
- `F4`: Workspace filter
- `F5/F6`: Time filters (from/to)
- `Shift+F3`: Scope to current result's agent
- `Shift+F4`: Clear workspace filter
- `Shift+F5`: Cycle presets (24h/7d/30d/all)
- `Ctrl+Del`: Clear all filters

**Modes:**
- `F2`: Toggle theme (6 presets)
- `F7`: Context window size (S/M/L/XL)
- `F9`: Match mode (prefix/standard)
- `F12`: Ranking mode
- `Ctrl+B`: Toggle border style

**Selection & Actions:**
- `m`: Toggle selection
- `Ctrl+A`: Select all
- `A`: Bulk actions menu
- `Ctrl+Enter`: Add to queue
- `Ctrl+O`: Open all queued
- `y`: Copy path/content
- `Ctrl+Y`: Copy all selected
- `/`: Find in detail pane
- `n/N`: Next/prev match

**Views & Palette:**
- `Ctrl+P`: Command palette
- `1-9`: Load saved view
- `Shift+1-9`: Save view to slot

**Source Filtering (multi-machine):**
- `F11`: Cycle source filter (all/local/remote)
- `Shift+F11`: Source selection menu

**Global:**
- `Ctrl+C`: Quit
- `F1` or `?`: Toggle help
- `Ctrl+Shift+R`: Force re-index
- `Ctrl+Shift+Del`: Reset all TUI state

### Detail Pane Tabs

| Tab | Content | Switch With |
|-----|---------|-------------|
| **Messages** | Full conversation with markdown | `[` / `]` |
| **Snippets** | Keyword-extracted summaries | `[` / `]` |
| **Raw** | Unformatted JSON/text | `[` / `]` |

### Context Window Sizing

| Size | Characters | Use Case |
|------|------------|----------|
| **Small** | ~200 | Quick scanning |
| **Medium** | ~400 | Default balanced view |
| **Large** | ~800 | Longer passages |
| **XLarge** | ~1600 | Full context, code review |

**Peek Mode** (`Ctrl+Space`): Temporarily expand to XL without changing default.

---

## Theme Presets

Cycle through 6 built-in themes with `F2`:

| Theme | Description | Best For |
|-------|-------------|----------|
| **Dark** | Tokyo Night-inspired deep blues | Low-light environments |
| **Light** | High-contrast light background | Bright environments |
| **Catppuccin** | Warm pastels, reduced eye strain | All-day coding |
| **Dracula** | Purple-accented dark theme | Popular developer theme |
| **Nord** | Arctic-inspired cool tones | Calm, focused work |
| **High Contrast** | Maximum readability | Accessibility needs |

All themes validated against WCAG contrast requirements (4.5:1 minimum for text).

### Role-Aware Message Styling

| Role | Visual Treatment |
|------|------------------|
| **User** | Blue-tinted background, bold |
| **Assistant** | Green-tinted background |
| **System** | Gray/muted background |
| **Tool** | Orange-tinted background |

---

## Saved Views

Save filter configurations to 9 slots for instant recall.

**What Gets Saved:**
- Active filters (agent, workspace, time range)
- Current ranking mode
- The search query

**Keyboard:**
- `Shift+1` through `Shift+9`: Save current view
- `1` through `9`: Load view from slot

**Via Command Palette:** `Ctrl+P` → "Save/Load view"

Views persist in `tui_state.json` across sessions.

---

## Density Modes

Control lines per search result. Cycle with `Shift+D`:

| Mode | Lines | Best For |
|------|-------|----------|
| **Compact** | 3 | Maximum results visible |
| **Cozy** | 5 | Balanced view (default) |
| **Spacious** | 8 | Detailed preview |

---

## Bookmark System

Save important results with notes and tags:

In TUI: Press `b` to bookmark, add notes and tags.

**Bookmark Structure:**
- `title`: Short description
- `source_path`, `line_number`, `agent`, `workspace`
- `note`: Your annotations
- `tags`: Comma-separated labels
- `snippet`: Extracted content

Storage: `~/.local/share/coding-agent-search/bookmarks.db` (SQLite)

---

## Optional Semantic Search

Local-only semantic search using MiniLM (no cloud):

**Required files** (place in data directory):
- `model.onnx`
- `tokenizer.json`
- `config.json`
- `special_tokens_map.json`
- `tokenizer_config.json`

Vector index stored as `vector_index/index-minilm-384.cvvi`.

CASS does NOT auto-download models; you must manually install them.

**Hash Embedder Fallback:** When MiniLM not installed, CASS uses a hash-based embedder for approximate semantic similarity.

---

## Watch Mode

Real-time index updates:

```bash
cass index --watch
```

- **Debounce:** 2 seconds (wait for burst to settle)
- **Max wait:** 5 seconds (force flush during continuous activity)
- **Incremental:** Only re-scans modified files

TUI automatically starts watch mode in background.

---

## Deduplication Strategy

CASS uses multi-layer deduplication:

1. **Message Hash**: SHA-256 of `(role + content + timestamp)` - identical messages stored once
2. **Conversation Fingerprint**: Hash of first N message hashes - detects duplicate files
3. **Search-Time Dedup**: Results deduplicated by content similarity

**Noise Filtering:**
- Empty messages and pure whitespace
- System prompts (unless searching for them)
- Repeated tool acknowledgments

---

## Performance Characteristics

| Operation | Latency |
|-----------|---------|
| Prefix search (cached) | 2-8ms |
| Prefix search (cold) | 40-60ms |
| Substring search | 80-200ms |
| Full reindex | 5-30s |
| Incremental reindex | 50-500ms |
| Health check | <50ms |

**Memory:** 70-140MB typical (50K messages)
**Disk:** ~600 bytes/message (including n-gram overhead)

---

## Response Shapes

**Search Response:**
```json
{
  "query": "error",
  "limit": 10,
  "count": 5,
  "total_matches": 42,
  "hits": [
    {
      "source_path": "/path/to/session.jsonl",
      "line_number": 123,
      "agent": "claude_code",
      "workspace": "/projects/myapp",
      "title": "Authentication debugging",
      "snippet": "The error occurs when...",
      "score": 0.85,
      "match_type": "exact",
      "created_at": "2024-01-15T10:30:00Z"
    }
  ],
  "_meta": {
    "elapsed_ms": 12,
    "cache_hit": true,
    "wildcard_fallback": false,
    "next_cursor": "eyJ...",
    "index_freshness": { "stale": false, "age_seconds": 120 }
  }
}
```

**Aggregation Response:**
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

---

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `CASS_DATA_DIR` | Override data directory |
| `CHATGPT_ENCRYPTION_KEY` | Base64 key for encrypted ChatGPT |
| `PI_CODING_AGENT_DIR` | Override Pi-Agent sessions path |
| `CASS_CACHE_SHARD_CAP` | Per-shard cache entries (default 256) |
| `CASS_CACHE_TOTAL_CAP` | Total cached hits (default 2048) |
| `CASS_DEBUG_CACHE_METRICS` | Enable cache debug logging |
| `CODING_AGENT_SEARCH_NO_UPDATE_PROMPT` | Skip update checks |

---

## Shell Completions

```bash
cass completions bash > ~/.local/share/bash-completion/completions/cass
cass completions zsh > "${fpath[1]}/_cass"
cass completions fish > ~/.config/fish/completions/cass.fish
cass completions powershell >> $PROFILE
```

---

## API Contract & Versioning

```bash
cass api-version --json
# → { "version": "0.4.0", "contract_version": "1", "breaking_changes": [] }

cass introspect --json
# → Full schema: all commands, arguments, response types
```

**Guaranteed Stable:**
- Exit codes and their meanings
- JSON response structure for `--robot` output
- Flag names and behaviors
- `_meta` block format

---

## Integration with CASS Memory (cm)

CASS provides **episodic memory** (raw sessions). CM extracts **procedural memory** (rules and playbooks):

```bash
# 1. CASS indexes raw sessions
cass index --full

# 2. Search for relevant past experience
cass search "authentication timeout" --robot --limit 10

# 3. CM reflects on sessions to extract rules
cm reflect
```

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| "missing index" | `cass index --full` |
| Stale warning | Rerun index or enable watch |
| Empty results | Check `cass stats --json`, verify connectors detected |
| JSON parsing errors | Use `--robot-format compact` |
| Watch not triggering | Check `watch_state.json`, verify file event support |
| Reset TUI state | `cass tui --reset-state` or `Ctrl+Shift+Del` |

---

## Installation

```bash
# One-liner install
curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/coding_agent_session_search/main/install.sh \
  | bash -s -- --easy-mode --verify

# Windows
irm https://raw.githubusercontent.com/Dicklesworthstone/coding_agent_session_search/main/install.ps1 | iex
```

---

## Integration with Flywheel

| Tool | Integration |
|------|-------------|
| **CM** | CASS provides episodic memory, CM extracts procedural memory |
| **NTM** | Robot mode flags for searching past sessions |
| **Agent Mail** | Search threads across agent history |
| **BV** | Cross-reference beads with past solutions |
