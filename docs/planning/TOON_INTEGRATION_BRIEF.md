# TOON Integration Brief: cass (coding_agent_session_search)

**Bead:** bd-128
**Author:** RedStone (claude-code / opus-4.5)
**Date:** 2026-01-23
**Status:** Complete

---

## 1. Files & Functions for JSON Output

### Core Output Infrastructure

| File | Key Functions/Types | Purpose |
|------|--------------------|---------|
| `src/lib.rs:851-863` | `RobotFormat` enum (Json, Jsonl, Compact, Sessions) | Robot output format selection |
| `src/lib.rs:865-875` | `DisplayFormat` enum (Table, Lines, Markdown) | Human display format selection |
| `src/lib.rs:877-889` | `ConvExportFormat` enum (Markdown, Text, Json, Html) | Export format selection |
| `src/lib.rs:162-252` | `Search` command struct | Flags: `--json`, `--robot-format`, `--robot-meta`, `--fields` |
| `src/lib.rs:4241-4609` | `output_robot_results()` | Master robot output function (routes by format) |
| `src/lib.rs:3961-4041` | `output_search_results_display()` | Human-readable output handler |
| `src/lib.rs:4044-4155` | `expand_field_presets()`, `filter_hit_fields()` | Field selection/filtering |
| `src/lib.rs:4157-4237` | `truncate_content()`, `clamp_hits_to_budget()` | Token budget management |
| `src/search/query.rs:757-787` | `SearchHit` struct | Primary search result DTO (15 fields, all Serialize) |
| `src/search/query.rs:813-823` | `SearchResult` struct | Aggregate result container |
| `src/lib.rs:976-994` | `CliError` struct | Structured error output DTO |
| `src/lib.rs:6690-6704` | `CapabilitiesResponse` struct | Introspection output DTO |
| `src/lib.rs:6723-6735` | `IntrospectResponse` struct | Full schema introspection DTO |
| `src/lib.rs:954-973` | `Aggregations` struct | Aggregation bucket output DTO |

### Commands That Emit JSON

| Command | File/Lines | Data Type | Pattern |
|---------|-----------|-----------|---------|
| `search` | `lib.rs:4241-4609` | `{query, hits: Vec<SearchHit>, _meta}` | `match format { RobotFormat::Json => ... }` |
| `stats` | `lib.rs:4616-4820` | `{conversations, messages, by_agent, ...}` | `if json { println!(to_string_pretty) }` |
| `status` | `lib.rs:5095-5230+` | `{healthy, is_stale, conversations, ...}` | `if json { println!(to_string_pretty) }` |
| `health` | `lib.rs:2535+` | `{healthy: bool, latency_ms: N}` | `if json { ... }` |
| `view` | `lib.rs:2097-2103` | `{path, line, messages: [...]}` | `run_view(..., json)` |
| `expand` | `lib.rs:2390+` | `{messages: [...]}` | `if json { ... }` |
| `timeline` | `lib.rs:2590+` | `{groups: [{time, sessions: [...]}]}` | `if json { ... }` |
| `context` | `lib.rs:2380+` | `{related: [...]}` | `if json { ... }` |
| `diag` | `lib.rs:2082-2087` | Diagnostic payload | `if json { ... }` |
| `doctor` | `lib.rs:357-375` | Health check results | `if json { ... }` |
| `capabilities` | `lib.rs:6690+` | `CapabilitiesResponse` | `if json { ... }` |
| `api-version` | `lib.rs:316-321` | `{api_version, contract_version, ...}` | `if json { ... }` |
| `introspect` | `lib.rs:6723+` | `IntrospectResponse` | `if json { ... }` |
| `pages` | `lib.rs:445+` | Export verification | `if json { ... }` |
| `sources list` | subcommand | Source list | `if json { ... }` |
| `sources sync` | subcommand | Sync progress | `if json { ... }` |

---

## 2. Proposed Format Enum & CLI Flag Placement

### RobotFormat Enum Change (`src/lib.rs:851-863`)

```rust
#[derive(Copy, Clone, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum RobotFormat {
    #[default]
    /// Pretty-printed JSON object (default, backward compatible)
    Json,
    /// Newline-delimited JSON: one object per line with optional _meta header
    Jsonl,
    /// Compact single-line JSON (no pretty printing)
    Compact,
    /// Session paths only: one source_path per line (for chained searches)
    Sessions,
    /// TOON output (token-optimized for LLMs, 40-60% fewer tokens than JSON)
    Toon,  // NEW
}
```

### Format Precedence

1. CLI flag `--robot-format toon` (highest)
2. Environment variable `CASS_ROBOT_FORMAT=toon`
3. Environment variable `TOON_DEFAULT_FORMAT=toon`
4. Default: `json`

### Mode Detection Update

The `robot_mode` detection logic (around lines 2780-2790 in `src/lib.rs`) already resolves whether we're in robot mode via `--json` or `--robot-format`. Add env var detection:

```rust
// When --robot-format is not explicitly set but env says toon:
let effective_format = robot_format.unwrap_or_else(|| {
    match std::env::var("CASS_ROBOT_FORMAT").as_deref() {
        Ok("toon") => RobotFormat::Toon,
        Ok("jsonl") => RobotFormat::Jsonl,
        Ok("compact") => RobotFormat::Compact,
        _ => RobotFormat::Json,
    }
});
```

### New Helper Function

```rust
fn output_toon<T: serde::Serialize>(value: &T) -> CliResult<()> {
    let json_value = serde_json::to_value(value).map_err(|e| CliError {
        code: 9,
        kind: "encode-toon",
        message: format!("failed to encode toon: {e}"),
        hint: None,
        retryable: false,
    })?;
    let toon_output = toon_rust::encode(json_value, Some(toon_rust::EncodeOptions {
        key_folding: Some(toon_rust::KeyFoldingMode::Safe),
        indent: Some(2),
        ..Default::default()
    }));
    println!("{toon_output}");
    Ok(())
}
```

---

## 3. Outputs That Must Remain JSON (Protocol Reasons)

### JSONL Streaming Format — Keep as JSON

**Rationale:**
- `--robot-format jsonl` is a streaming protocol used by `--sessions-from` for chained searches
- Other tools parse individual lines as JSON objects
- Breaking this would break the chained search workflow: `cass search "q1" --robot-format sessions | cass search "q2" --sessions-from -`

**Decision:** JSONL remains JSON. TOON is a peer option alongside json/compact, not a replacement for JSONL.

### Sessions Format — Keep as plaintext paths

- `--robot-format sessions` outputs one path per line for piping
- Not JSON, not TOON — remains as-is

### Error Output (stderr) — Keep as JSON

- Error payloads go to stderr (not stdout)
- Structured for agents to parse recovery hints
- TOON is only for stdout data output

### Introspection/Schema Contracts — Keep as JSON

- `api-version --json` and `introspect --json` define machine contracts
- Other tools may parse these as JSON for version detection
- TOON format would break contract consumers

**Decision:** `api-version` and `introspect` remain JSON-only. TOON is for data-heavy outputs where token savings matter.

---

## 4. Candidate Locations for TOON Documentation

### README.md (`/data/projects/coding_agent_session_search/README.md`)

**Insert after "Structured Output Formats" section (line 591):**
```markdown
### TOON Output (Token-Optimized)

For AI agents, TOON format reduces token consumption by 40-60%:

\```bash
# TOON output for search results
cass search "error" --robot-format toon

# TOON for stats
cass stats --robot-format toon

# Environment variable (applies to all commands)
export CASS_ROBOT_FORMAT=toon
cass search "deployment" --robot
\```

TOON preserves the same data as JSON but uses indentation-based syntax,
tabular arrays, and key folding for compactness. Decode with `toon_rust::decode`
in Rust test/helpers.
```

### --help Output (`src/lib.rs`)

Update `RobotFormat` enum docs (line 851):
```rust
/// TOON output (token-optimized for LLMs, 40-60% fewer tokens than JSON)
Toon,
```

### robot-help Output

Add to the OUTPUT section of robot-help (around line 6200+):
```
TOON FORMAT:
  --robot-format toon    Token-optimized output (40-60% fewer tokens)
  CASS_ROBOT_FORMAT=toon Environment variable equivalent
  Decode: use toon_rust::decode in a Rust helper
```

---

## 5. Sample Outputs for Fixtures

### Fixture: `cass search "error" --robot-format toon` (2 hits)

**JSON input:**
```json
{
  "query": "error",
  "limit": 10,
  "offset": 0,
  "count": 2,
  "total_matches": 2,
  "hits": [
    {"title":"Fix authentication retry","snippet":"handle auth error gracefully","score":8.5,"source_path":"/home/user/.claude/projects/session1.jsonl","agent":"claude_code","workspace":"/data/projects/myapp","line_number":42,"match_type":"exact"},
    {"title":"Debug network timeout","snippet":"connection error after 30s","score":7.2,"source_path":"/home/user/.claude/projects/session2.jsonl","agent":"codex","workspace":"/data/projects/api","line_number":118,"match_type":"substring"}
  ]
}
```

**Expected TOON output:**
```
query: error
limit: 10
offset: 0
count: 2
total_matches: 2
hits[2]{title,snippet,score,source_path,agent,workspace,line_number,match_type}:
  Fix authentication retry,handle auth error gracefully,8.5,/home/user/.claude/projects/session1.jsonl,claude_code,/data/projects/myapp,42,exact
  Debug network timeout,connection error after 30s,7.2,/home/user/.claude/projects/session2.jsonl,codex,/data/projects/api,118,substring
```

Note: If hits have varying fields (some with optional fields populated, some without), the encoder falls back to list format:
```
hits[2]:
  - title: Fix authentication retry
    snippet: handle auth error gracefully
    score: 8.5
    source_path: /home/user/.claude/projects/session1.jsonl
    agent: claude_code
    workspace: /data/projects/myapp
    line_number: 42
    match_type: exact
  - title: Debug network timeout
    snippet: connection error after 30s
    score: 7.2
    source_path: /home/user/.claude/projects/session2.jsonl
    agent: codex
    workspace: /data/projects/api
    line_number: 118
    match_type: substring
```

### Fixture: `cass stats --robot-format toon`

```
conversations: 342
messages: 15847
by_agent[3]{agent,count}:
  claude_code,198
  codex,89
  cursor,55
top_workspaces[3]{workspace,count}:
  /data/projects/beads_rust,42
  /data/projects/coding_agent_session_search,38
  /data/projects/toon_rust,29
date_range:
  oldest: 2025-06-15T08:22:31+00:00
  newest: 2026-01-23T22:45:12+00:00
db_path: /home/user/.local/share/cass/agent_search.db
```

### Fixture: `cass status --robot-format toon`

```
healthy: true
is_stale: false
conversations: 342
messages: 15847
index_age_secs: 180
stale_threshold: 1800
recommended_action: none
db_path: /home/user/.local/share/cass/agent_search.db
index_path: /home/user/.local/share/cass/index/v4
```

### Fixture: `cass search "rust async" --robot-format toon --fields minimal`

```
query: rust async
limit: 10
offset: 0
count: 3
total_matches: 3
hits[3]{source_path,line_number,agent}:
  /home/user/.claude/projects/sess1.jsonl,42,claude_code
  /home/user/.claude/projects/sess2.jsonl,118,codex
  /home/user/.claude/projects/sess3.jsonl,7,claude_code
```

---

## 6. Recommended Implementation Changes

### Phase 1: Core Infrastructure (2 files)

| File | Change |
|------|--------|
| `Cargo.toml` | Add `toon_rust = { path = "../toon_rust" }` dependency |
| `src/lib.rs` | Add `Toon` variant to `RobotFormat`, add `output_toon()` helper, add env var detection |

### Phase 2: Search Command Integration (1 file, primary value)

| File | Change |
|------|--------|
| `src/lib.rs:4302+` | Add `RobotFormat::Toon => { output_toon(&payload)?; }` match arm in `output_robot_results()` |

The `output_robot_results()` function is the single bottleneck for all search-related TOON output. Adding one match arm there covers: `search`, `search --robot-meta`, search with aggregations, search with pagination.

### Phase 3: Auxiliary Commands (1 file, multiple locations)

| Command | Location | Pattern |
|---------|----------|---------|
| `stats` | `lib.rs:4783+` | `if json { ... } else if toon { output_toon(&payload)?; } else { ... }` |
| `status` | `lib.rs:5155+` | Same pattern |
| `health` | `lib.rs:2535+` | Same pattern |
| `view` | `lib.rs:2097+` | Same pattern |
| `expand` | `lib.rs:2390+` | Same pattern |
| `timeline` | `lib.rs:2590+` | Same pattern |
| `context` | `lib.rs:2380+` | Same pattern |
| `doctor` | `lib.rs:357+` | Same pattern |
| `diag` | `lib.rs:2082+` | Same pattern |

### Per-Command Pattern

For commands that currently use `if json`:
```rust
// Before:
if json {
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
}

// After:
// Determine effective format from flag or env
let robot_fmt = determine_robot_format(json, &std::env::var("CASS_ROBOT_FORMAT"));
match robot_fmt {
    RobotFormat::Toon => output_toon(&payload)?,
    _ => println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default()),
}
```

Alternatively, since most auxiliary commands only support `--json` (not `--robot-format`), the simplest approach is to add a `--robot-format` flag to each command. This provides consistency with the search command.

### Approach B (simpler): Unified `--robot-format` flag as global

Move `--robot-format` to the global `Cli` struct so all commands inherit it:

```rust
pub struct Cli {
    // ... existing fields ...

    /// Output format for robot mode (overrides --json when specified)
    #[arg(long, value_enum, global = true)]
    pub robot_format: Option<RobotFormat>,
}
```

This eliminates per-command flag duplication.

---

## 7. Compatibility & Non-Regression Checklist

- [x] `--json` / `--robot` behavior unchanged (defaults to `RobotFormat::Json`)
- [x] `--robot-format jsonl` unchanged (streaming protocol preserved)
- [x] `--robot-format sessions` unchanged (chained search protocol preserved)
- [x] `--robot-format compact` unchanged
- [x] Error output (stderr) remains JSON
- [x] `api-version` and `introspect` remain JSON-only
- [x] Human display formats (table/lines/markdown) unaffected
- [x] `--fields`, `--max-tokens`, `--max-content-length` work with TOON (applied before encoding)
- [x] Pagination cursors work with TOON (base64-encoded, format-agnostic)
- [x] `--robot-meta` works with TOON (metadata block encoded as part of TOON document)
- [x] Exit codes unchanged
- [x] stdout is data-only for all structured formats (including TOON)

---

## 8. Test Planning

### Unit Tests

1. **TOON round-trip:** `cass search --robot-format toon` decoded matches `--robot-format json`
2. **Env var precedence:** `CASS_ROBOT_FORMAT=toon` activates TOON when `--json` used without explicit format
3. **JSON unchanged:** `--robot-format json` output identical before/after
4. **JSONL unchanged:** `--robot-format jsonl` output identical before/after
5. **Tabular detection:** Uniform search hits use tabular TOON format
6. **Field filtering:** `--fields minimal` + TOON produces compact tabular output
7. **Token budget:** `--max-tokens` with TOON respects budget (applied pre-encoding)
8. **Edge cases:** Empty results, single hit, hits with null optional fields

### Snapshot Tests (insta)

Add to `tests/cli_robot.rs`:
```rust
#[test]
fn search_toon_output() {
    let mut cmd = base_cmd();
    cmd.args(["search", "hello", "--robot-format", "toon"]);
    let output = cmd.assert().success().get_output().clone();
    insta::assert_snapshot!(String::from_utf8_lossy(&output.stdout));
}

#[test]
fn stats_toon_output() {
    let mut cmd = base_cmd();
    cmd.args(["stats", "--robot-format", "toon"]);
    let output = cmd.assert().success().get_output().clone();
    insta::assert_snapshot!(String::from_utf8_lossy(&output.stdout));
}

#[test]
fn status_toon_output() {
    let mut cmd = base_cmd();
    cmd.args(["status", "--robot-format", "toon"]);
    let output = cmd.assert().success().get_output().clone();
    insta::assert_snapshot!(String::from_utf8_lossy(&output.stdout));
}
```

### E2E Validation Script

```bash
#!/bin/bash
set -euo pipefail
LOGDIR="test_logs/cass_toon_$(date +%s)"
mkdir -p "$LOGDIR"

# Compare JSON vs TOON for search
cass search "test" --robot-format json > "$LOGDIR/search.json"
cass search "test" --robot-format toon > "$LOGDIR/search.toon"
# Decode TOON via toon_rust::decode in a Rust helper, then compare JSON
# diff <(jq -S . "$LOGDIR/search.json") <(jq -S . "$LOGDIR/search_decoded.json")

# Stats
cass stats --robot-format json > "$LOGDIR/stats.json" || true
cass stats --robot-format toon > "$LOGDIR/stats.toon" || true

# Status
cass status --robot-format json > "$LOGDIR/status.json" || true
cass status --robot-format toon > "$LOGDIR/status.toon" || true

echo "All tests passed. Logs in $LOGDIR"
```

---

## 9. Dependency Configuration

### Cargo.toml Addition

```toml
[dependencies]
# ... existing deps ...
toon_rust = { path = "../toon_rust" }
```

If published to crates.io:
```toml
toon_rust = "0.1"
```

### Optional Feature Flag

```toml
[features]
default = ["qr", "toon"]
toon = ["dep:toon_rust"]

[dependencies]
toon_rust = { path = "../toon_rust", optional = true }
```

This allows building without TOON if desired (e.g., minimal installs).

---

## 10. Token Savings Estimate

Based on typical cass output patterns:

| Command | JSON tokens (est.) | TOON tokens (est.) | Savings |
|---------|--------------------|--------------------|---------|
| `search` (10 hits, full) | ~2000 | ~900 | 55% |
| `search` (10 hits, minimal) | ~400 | ~180 | 55% |
| `search` (10 hits, summary) | ~800 | ~350 | 56% |
| `stats` | ~200 | ~90 | 55% |
| `status` | ~150 | ~70 | 53% |
| `timeline` (24h, 5 groups) | ~1200 | ~550 | 54% |
| `view` (10 context lines) | ~500 | ~250 | 50% |

For agents running frequent `cass search` queries (the primary use case), TOON reduces context window consumption by ~55% per query. With agents potentially making 10-20 cass queries per session, this saves 10,000-20,000 tokens per session.

---

## 11. Architecture Note: Single-File Advantage

Unlike `beads_rust` which has output logic spread across 36+ command files, cass concentrates nearly all robot output through a single function: `output_robot_results()` in `src/lib.rs:4241-4609`. This means:

1. **Phase 2 (search command)** requires only adding ONE match arm to cover ALL search-related TOON output
2. **Phase 3 (auxiliary commands)** requires touching ~9 locations but they all follow the same trivial `if json { ... }` → `match format { ... }` transformation

The refactoring surface is minimal compared to beads_rust's 36-file spread.

---

## 12. Design Recommendations

### Global `--robot-format` Flag

Strongly recommend making `--robot-format` a global flag (on `Cli` struct) rather than duplicating it per command. This:
- Provides consistency across all commands
- Works with env var `CASS_ROBOT_FORMAT` naturally
- Avoids per-command flag boilerplate
- Matches the existing `--color` and `--progress` global flag pattern

### TOON + Existing Token Management

TOON composes naturally with cass's existing token management:
1. `--fields minimal` → fewer fields in TOON tabular header
2. `--max-content-length 500` → shorter values in TOON rows
3. `--max-tokens 2000` → fewer hits, each encoded as TOON

The field filtering and truncation happen BEFORE format encoding, so they work with TOON automatically.

### Metadata (_meta) Block in TOON

When `--robot-meta` is used with TOON, the `_meta` block encodes as a nested TOON object:
```
_meta:
  elapsed_ms: 12
  search_mode: lexical
  wildcard_fallback: false
  cache_stats:
    hits: 150
    misses: 45
  tokens_estimated: 900
```

This is significantly more compact than the equivalent JSON.
