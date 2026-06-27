# CLI & Robot Mode Enhancements

## Overview

This bead collection focuses on making `cass` more powerful and useful for both
human CLI users and AI agents consuming robot mode output. All improvements are
**self-contained** (no external APIs, embeddings, or LLM dependencies) and build
on existing Tantivy/SQLite infrastructure.

### Design Philosophy

1. **Robot mode is the API** - AI agents will call `cass search --robot`, so this
   must be rock-solid, predictable, and well-documented
2. **CLI should be powerful without TUI** - Users shouldn't need to enter TUI for
   quick searches; pipe-friendly output matters
3. **Query language should be expressive** - Boolean operators and field syntax
   unlock power-user and agent workflows
4. **Backward compatibility** - Existing robot mode consumers shouldn't break

### Success Criteria

- AI agents can reliably parse all robot mode output
- Complex queries are expressible in a single command
- Output is predictable and scriptable
- No performance regression

---

## Dependency Graph (REVISED)

```
┌─────────────────────────────────────────────────────────────────────┐
│                     ALREADY IMPLEMENTED                              │
│  cre.1: Quiet/Verbose (--quiet, --verbose exist)                    │
│  cre.7: Date Range Flags (--today, --week, --since, --until exist)  │
└─────────────────────────────────────────────────────────────────────┘

Independent beads (can be done in parallel):

    ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
    │ cre.2: Robot    │   │ cre.3: Human    │   │ cre.9: Diagnose │
    │ Output Enhance  │   │ Output (--disp) │   │ (health focus)  │
    └────────┬────────┘   └─────────────────┘   └─────────────────┘
             │
             ▼
    ┌─────────────────┐
    │ cre.4: Boolean  │ ←── Independent, high value
    │ Query Operators │
    └────────┬────────┘
             │
             ▼
    ┌─────────────────┐
    │ cre.5: Field    │ ←── P2: nice-to-have (--agent/--workspace exist)
    │ Syntax          │
    └────────┬────────┘
             │
             ▼
    ┌─────────────────┐
    │ cre.10: Dynamic │ ←── P3: polish
    │ Shell Complete  │
    └─────────────────┘

Standalone beads (no dependencies):

    ┌─────────────────┐   ┌─────────────────┐
    │ cre.6: Export   │   │ cre.8: Context  │
    │ (simplified)    │   │ -C (like grep)  │
    └─────────────────┘   └─────────────────┘
```

---

## BEAD cre.1: Quiet/Verbose Output Modes

**Priority:** P0 (foundational)
**Complexity:** Low
**Dependencies:** None
**Status:** ✅ MOSTLY IMPLEMENTED

### What Already Exists

The following flags are already implemented in `src/lib.rs`:
- `--quiet / -q` - Sets log filter to "warn" level
- `--verbose / -v` - Sets log filter to "debug" level (just added)

The tracing crate already provides `info!`, `debug!`, `warn!` macros that respect
the log filter.

### Remaining Work

1. **Verify behavior**: Ensure robot mode outputs clean JSON on stdout with no
   stderr pollution (except actual errors)
2. **Update documentation**: Add --verbose to robot-docs output
3. **Test coverage**: Add tests for output stream behavior

### Subtasks

- [x] cre.1.1: Add --quiet and --verbose to clap args (DONE)
- [x] cre.1.2: Uses existing tracing macros (info!/debug!/warn!) - DONE
- [ ] cre.1.3: Verify robot mode stdout/stderr separation
- [ ] cre.1.4: Update --robot-help to mention --verbose
- [ ] cre.1.5: Add tests for output stream behavior

---

## BEAD cre.2: Enhanced Robot Mode Output

**Priority:** P0 (critical for AI agents)
**Complexity:** Medium
**Dependencies:** None (cre.1 mostly done)

### Background

Robot mode (`--robot`) outputs JSON for programmatic consumption. Current format:
```json
{"query": "...", "limit": 10, "offset": 0, "count": 5, "hits": [...]}
```

### What Already Exists

- `--offset N` and `--limit N` for pagination (already implemented)
- JSON output with pretty printing
- Error output as JSON to stderr

### Issues to Address

1. **No JSONL option** for streaming large result sets
2. **No timing metadata** (elapsed_ms)
3. **No indication of wildcard fallback** being used

### Requirements (REVISED - backward compatible)

1. **Format options**: `--robot-format json|jsonl|compact`
   - `json`: Current behavior (default, unchanged)
   - `jsonl`: One JSON object per line (streaming-friendly)
   - `compact`: Current structure, minimal whitespace

2. **Optional metadata**: `--robot-meta` flag adds extra fields
   ```json
   {
     "query": "...",
     "limit": 10,
     "offset": 0,
     "count": 5,
     "elapsed_ms": 45,           // NEW (only with --robot-meta)
     "wildcard_fallback": false, // NEW (only with --robot-meta)
     "hits": [...]
   }
   ```
   Note: Fields added at top level, not wrapped in "meta" to avoid breaking change.

3. **JSONL format**: Each result on its own line
   ```
   {"_meta": {"query": "...", "count": 5, "elapsed_ms": 45}}
   {"score": 0.95, "agent": "claude", ...}
   {"score": 0.87, "agent": "codex", ...}
   ```
   First line is metadata (prefixed with `_meta` key), then each hit.

**REMOVED** (not worth complexity):
- `--fields` flag - agents can filter JSON themselves; adds parsing complexity
- Metadata envelope wrapper - would break existing consumers

### Implementation Notes

```rust
#[derive(clap::ValueEnum, Default)]
enum RobotFormat {
    #[default]
    Json,    // Pretty JSON (current)
    Jsonl,   // Streaming, one object per line
    Compact, // Single-line JSON
}

// In search output:
if robot_meta {
    payload["elapsed_ms"] = elapsed.as_millis();
    payload["wildcard_fallback"] = wildcard_fallback;
}
```

### Subtasks

- [ ] cre.2.1: Add --robot-format enum (json, jsonl, compact)
- [ ] cre.2.2: Add --robot-meta flag for extended metadata
- [ ] cre.2.3: Implement JSONL streaming output with _meta header
- [ ] cre.2.4: Implement compact (minified) JSON output
- [ ] cre.2.5: Track and report elapsed_ms in robot output
- [ ] cre.2.6: Track and report wildcard_fallback in robot output
- [ ] cre.2.7: Update --robot-help with new options
- [ ] cre.2.8: Add tests for each format

---

## BEAD cre.3: Human-Readable CLI Output Modes

**Priority:** P1
**Complexity:** Medium
**Dependencies:** None (independent of robot mode)

### Background

Users shouldn't need to enter TUI for quick searches. A readable CLI output mode
enables workflows like:
```bash
cass search "auth bug" --display table | head -20
cass search "config" --display lines | wc -l
cass search "error" --display markdown >> notes.md
```

### What Already Exists

- Basic text output format (score/agent/workspace/snippet)
- `--color auto|always|never` flag

### Requirements (REVISED - use --display to avoid conflict with --robot-format)

1. **Flag name**: `--display` (not `--format`, which is for robot mode)

2. **Default format**: Current behavior (separator lines + multi-field output)

3. **Table format**: `--display table`
   ```
   SCORE  AGENT   WORKSPACE            TITLE
   0.95   claude  /home/user/project   Fix auth flow
   0.87   codex   /home/user/other     Refactor login
   ```

4. **Lines format**: `--display lines` (compact, one-liner per result)
   ```
   [0.95] claude:/home/user/project "Fix auth flow" - First 60 chars...
   ```

5. **Markdown format**: `--display markdown`
   ```markdown
   ## Search: "auth bug"

   ### Fix auth flow
   - **Agent:** claude
   - **Score:** 0.95
   - **Path:** /home/user/project/.claude/...

   > Snippet text here...
   ```

6. **Terminal width awareness**: Auto-truncate to fit terminal

### Implementation Notes

```rust
#[derive(clap::ValueEnum, Default)]
enum DisplayFormat {
    #[default]
    Default, // Current separator-based format
    Table,   // Aligned columns
    Lines,   // One-liner per result
    Markdown, // For documentation
}

// Add to search command
#[arg(long, value_enum, default_value_t = DisplayFormat::Default)]
display: DisplayFormat,
```

Reuse `contextual_snippet` from TUI for snippet generation.

### Subtasks

- [ ] cre.3.1: Add --display enum (default, table, lines, markdown)
- [ ] cre.3.2: Implement table formatter with column width calculation
- [ ] cre.3.3: Implement lines (compact one-line) formatter
- [ ] cre.3.4: Implement markdown formatter
- [ ] cre.3.5: Auto-detect terminal width for truncation
- [ ] cre.3.6: Ensure --display is ignored when --robot is set
- [ ] cre.3.7: Add tests for each format

---

## BEAD cre.4: Boolean Query Operators

**Priority:** P1 (high value for power users and agents)
**Complexity:** Medium-High (requires proper parser)
**Dependencies:** None

### Background

Current query handling in `sanitize_query` strips all non-alphanumeric characters
except `*`. This prevents boolean expressions. Tantivy supports boolean queries
natively; we just need to parse and construct them.

### Requirements

1. **AND operator**: `auth AND login` (both terms required)
   - Implicit AND is default for multiple terms (current behavior)
   - Explicit AND for clarity

2. **OR operator**: `error OR exception` (either term)

3. **NOT operator**: `config NOT deprecated`
   - Also support `-term` syntax: `config -deprecated`

4. **Grouping**: `(auth OR login) AND error`

5. **Quoted phrases**: `"exact phrase match"`
   - Use Tantivy PhraseQuery

6. **Backward compatibility**: Simple queries work unchanged

### Implementation Notes

Create a proper query parser using recursive descent or Pratt parsing.
Consider the `logos` crate for lexing. The output is Tantivy `Box<dyn Query>`.

**Query Grammar:**
```
query     = or_expr
or_expr   = and_expr (OR and_expr)*
and_expr  = unary_expr (AND? unary_expr)*  // AND is implicit between terms
unary_expr = NOT? primary
primary   = TERM | PHRASE | WILDCARD | '(' query ')'
TERM      = [a-zA-Z0-9_]+
PHRASE    = '"' [^"]+ '"'
WILDCARD  = '*'? TERM '*'?
```

```rust
enum ParsedQuery {
    Term(String),
    Phrase(Vec<String>),
    And(Box<ParsedQuery>, Box<ParsedQuery>),
    Or(Box<ParsedQuery>, Box<ParsedQuery>),
    Not(Box<ParsedQuery>),
    Wildcard(WildcardPattern),
}

fn parse_query(input: &str) -> Result<ParsedQuery, QueryParseError>;
fn to_tantivy_query(parsed: &ParsedQuery, schema: &Schema) -> Box<dyn Query>;
```

### Subtasks

- [ ] cre.4.1: Design and document query grammar
- [ ] cre.4.2: Implement lexer/tokenizer for query string
- [ ] cre.4.3: Implement recursive descent parser
- [ ] cre.4.4: Implement phrase query parsing ("quoted text")
- [ ] cre.4.5: Implement NOT and -term parsing
- [ ] cre.4.6: Implement grouping with parentheses
- [ ] cre.4.7: Convert ParsedQuery to Tantivy BooleanQuery
- [ ] cre.4.8: Preserve existing wildcard support (*term*)
- [ ] cre.4.9: Add comprehensive parser tests (20+ cases)
- [ ] cre.4.10: Update help text with query syntax docs

---

## BEAD cre.5: Field-Specific Search Syntax

**Priority:** P2 (nice-to-have - CLI flags already cover main use cases)
**Complexity:** Medium
**Dependencies:** cre.4 (builds on boolean parser)

### Background

Users currently filter by agent/workspace via separate flags (`--agent`, `--workspace`).
Inline syntax like `agent:claude` is more ergonomic for complex queries but is
**not essential** since the flags work fine.

### What Already Exists

- `--agent <slug>` flag for filtering by agent
- `--workspace <path>` flag for filtering by workspace

### When This Becomes Valuable

The inline syntax is mainly useful when combined with boolean operators:
```bash
cass search "agent:claude AND (auth OR login)"
cass search "(agent:claude OR agent:codex) AND error"
```

Without boolean operators (cre.4), field syntax is redundant with existing flags.

### Requirements (simplified)

1. **Field prefixes**: `field:value` syntax
   - `agent:claude` - filter by agent
   - `workspace:/path` - filter by workspace path
   - `title:foo` - search only in title field
   - `content:bar` - search only in content field

2. **Negation**: `-agent:codex` (exclude agent)

3. **Integration with boolean operators**:
   `agent:claude AND (auth OR login)`

**DEFERRED** (add complexity, limited value):
- Multiple values (`agent:claude,codex`) - just use OR
- Wildcards in values - use existing * syntax
- File pattern matching - too niche

### Implementation Notes

Integrate into the boolean parser from cre.4 rather than extracting beforehand.

```rust
// Extend ParsedQuery enum:
enum ParsedQuery {
    // ... existing variants ...
    Field { name: String, value: String, negated: bool },
}
```

### Subtasks

- [ ] cre.5.1: Define supported field names (agent, workspace, title, content)
- [ ] cre.5.2: Extend lexer to recognize field:value tokens
- [ ] cre.5.3: Extend parser to handle field prefixes
- [ ] cre.5.4: Handle negated field prefixes (-agent:)
- [ ] cre.5.5: Convert field nodes to Tantivy TermQuery
- [ ] cre.5.6: Add tests for field syntax
- [ ] cre.5.7: Document field syntax in help

---

## BEAD cre.6: Conversation Export

**Priority:** P2
**Complexity:** Low
**Dependencies:** None (standalone feature)

### Background

Users want to export conversations for documentation, sharing, or archival.
Currently must view in TUI or parse JSON output manually.

### Requirements (simplified - single export first)

1. **Export command**: `cass export <source-path>`
   - Takes a source_path from search results
   - Outputs to stdout by default

2. **Output flag**: `--output file.md` writes to file instead of stdout

3. **Formats**: `--format markdown|text|json`
   - `markdown` (default): Role headers, code blocks preserved
   - `text`: Plain text, no formatting
   - `json`: Raw JSON structure

4. **Markdown output**:
   ```markdown
   # Conversation: Fix authentication bug

   **Agent:** claude
   **Workspace:** /home/user/myproject
   **Date:** 2024-01-15 14:30

   ---

   ## User

   I need help fixing the auth bug...

   ## Assistant

   I'll help you fix that. Let me look at the code...

   ```python
   def authenticate(user):
       ...
   ```
   ```

5. **Robot mode**: `cass export <path> --robot` outputs JSON

**DEFERRED** (add later if needed):
- Batch export (`--all --output-dir`) - adds complexity
- Template customization - YAGNI

### Implementation Notes

Reuse conversation loading from existing connectors and SQLite storage.

```rust
// In Commands enum:
Export {
    path: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = ExportFormat::Markdown)]
    format: ExportFormat,
    #[arg(long)]
    robot: bool,
}
```

### Subtasks

- [ ] cre.6.1: Add export subcommand to CLI
- [ ] cre.6.2: Implement conversation loading by source path
- [ ] cre.6.3: Implement markdown formatter for conversations
- [ ] cre.6.4: Implement text (plain) formatter
- [ ] cre.6.5: Handle code block detection/preservation
- [ ] cre.6.6: Add --output flag for file output
- [ ] cre.6.7: Add --robot flag for JSON output
- [ ] cre.6.8: Add tests for export formats

---

## BEAD cre.7: Date Range CLI Flags

**Priority:** N/A
**Complexity:** N/A
**Dependencies:** N/A
**Status:** ✅ ALREADY IMPLEMENTED

### What Already Exists (in src/lib.rs lines 134-150)

The following date filtering flags are already implemented:

1. **Shortcut flags**:
   - `--today` - Filter to today only
   - `--yesterday` - Filter to yesterday only
   - `--week` - Filter to last 7 days
   - `--days N` - Filter to last N days

2. **Absolute dates**:
   - `--since YYYY-MM-DD` or `--since YYYY-MM-DDTHH:MM:SS`
   - `--until YYYY-MM-DD` or `--until YYYY-MM-DDTHH:MM:SS`

The `TimeFilter` struct and `parse_datetime_str` function handle ISO 8601 parsing.

### Potential Future Enhancement

If natural language parsing is desired ("3 days ago", "last week"), that would
be a new feature. But the current implementation covers 95% of use cases.

### Subtasks

- [x] cre.7.1: Add --since and --until CLI flags (DONE)
- [x] cre.7.2: Implement ISO 8601 date parsing (DONE)
- [x] cre.7.3: Add --today, --week, --days shortcuts (DONE)
- [x] cre.7.4: Integrate with SearchFilters (DONE)
- [ ] cre.7.5: (OPTIONAL) Natural language date parsing
- [ ] cre.7.6: Include resolved dates in robot mode metadata (part of cre.2)

---

## BEAD cre.8: Search Context (-C)

**Priority:** P2
**Complexity:** Medium
**Dependencies:** None (standalone feature)

### Background

Like `grep -C`, users want to see messages before/after the match to understand
context. Current results show only the matching message.

### Requirements

1. **Context flag**: `--context N` or `-C N`
   - Show N messages before and after match

2. **Directional**: `--before N` (`-B`), `--after N` (`-A`)

3. **Output format**: Clearly delineate context vs match
   ```
   [context] User: Earlier message...
   [context] Assistant: Previous response...
   [MATCH]   User: The matching message
   [context] Assistant: Following response...
   ```

4. **Robot mode**: Include context messages in output
   ```json
   {
     "match": {...},
     "context_before": [...],
     "context_after": [...]
   }
   ```

### Performance Consideration

Loading full conversations for every result could be slow. Options:
1. Only fetch context for top N results (default 5)
2. Make context opt-in per result (TUI already does this in detail view)
3. Lazy load context only when --context is specified

Recommended: Option 3 (only load when -C flag is present)

### Implementation Notes

Use `msg_idx` field from SearchHit to locate match position, then load
surrounding messages from SQLite.

```rust
// Add to search command:
#[arg(short = 'C', long)]
context: Option<usize>,

#[arg(short = 'B', long)]
before: Option<usize>,

#[arg(short = 'A', long)]
after: Option<usize>,
```

### Subtasks

- [ ] cre.8.1: Add -C, -B, -A flags to search command
- [ ] cre.8.2: Implement context loading from SQLite by conversation_id + msg_idx
- [ ] cre.8.3: Extract context window around match
- [ ] cre.8.4: Format context in CLI text output
- [ ] cre.8.5: Format context in robot mode JSON
- [ ] cre.8.6: Handle edge cases (match at start/end of conversation)
- [ ] cre.8.7: Add tests for context extraction

---

## BEAD cre.9: Diagnostic Mode

**Priority:** P2
**Complexity:** Low
**Dependencies:** None

### Background

When things go wrong, users need visibility into index health. The existing
`cass stats` command shows counts, but doesn't check health or detect issues.

### Differentiation from `cass stats`

| Feature                | `cass stats`          | `cass diagnose`       |
|------------------------|----------------------|----------------------|
| Document counts        | ✅                    | ✅                    |
| Agent breakdown        | ✅                    | ✅                    |
| Workspace breakdown    | ✅                    | ❌                    |
| Index readable check   | ❌                    | ✅ **NEW**            |
| Schema version match   | ❌                    | ✅ **NEW**            |
| Connector detection    | ❌                    | ✅ **NEW**            |
| Issue detection        | ❌                    | ✅ **NEW**            |
| Fix suggestions        | ❌                    | ✅ **NEW**            |
| Disk usage             | ❌                    | ✅ **NEW**            |

**Key insight**: `stats` is for "how much data do I have?" while `diagnose` is
for "is everything working correctly?"

### Requirements

1. **Diagnose command**: `cass diagnose`

2. **Health checks** (the main value):
   - Can Tantivy index be opened?
   - Can SQLite database be opened?
   - Does schema version match expected?
   - Are connector roots accessible?
   - Any orphaned data (index vs SQLite mismatch)?

3. **Disk usage**:
   - Index size on disk
   - SQLite file size
   - Total data directory size

4. **Issue detection with suggestions**:
   ```
   ⚠️  ISSUE: Schema version mismatch (expected v4, found v3)
   💡 FIX: Run `cass index --full --force-rebuild` to rebuild index

   ⚠️  ISSUE: Claude Code directory not found (~/.claude)
   💡 FIX: This is normal if you don't use Claude Code
   ```

5. **Robot mode**: `cass diagnose --robot` outputs JSON

### Implementation Notes

```rust
#[derive(Serialize)]
struct DiagnosticReport {
    status: DiagnosticStatus, // Healthy, Warning, Error
    checks: Vec<HealthCheck>,
    disk_usage: DiskUsage,
    connectors: Vec<ConnectorStatus>,
    issues: Vec<DiagnosticIssue>,
}

#[derive(Serialize)]
struct DiagnosticIssue {
    severity: Severity, // Warning, Error
    message: String,
    suggestion: Option<String>,
}
```

### Subtasks

- [ ] cre.9.1: Add diagnose subcommand
- [ ] cre.9.2: Implement index health check (can open?)
- [ ] cre.9.3: Implement SQLite health check (can open?)
- [ ] cre.9.4: Implement schema version check
- [ ] cre.9.5: Implement connector root detection
- [ ] cre.9.6: Implement disk usage calculation
- [ ] cre.9.7: Format human-readable output with colors
- [ ] cre.9.8: Format robot mode JSON output
- [ ] cre.9.9: Add fix suggestions for common issues

---

## BEAD cre.10: Enhanced Shell Completions

**Priority:** P3
**Complexity:** Medium
**Dependencies:** cre.5

### Background

clap_complete provides static completions. Context-aware completions (agent names,
workspace paths) would significantly improve CLI UX.

### Requirements

1. **Dynamic agent completion**: Complete agent names from index
   ```bash
   cass search agent:<TAB>
   # claude  codex  gemini  cline
   ```

2. **Workspace completion**: Complete known workspace paths
   ```bash
   cass search workspace:<TAB>
   # /home/user/project1  /home/user/project2
   ```

3. **Field name completion**: Complete valid field prefixes
   ```bash
   cass search <TAB>
   # agent:  workspace:  title:  content:  file:
   ```

4. **Support major shells**: bash, zsh, fish

### Implementation Notes

Implement a `cass completions --generate-dynamic` that queries the index
and outputs completion data. Shell scripts source this.

For zsh, use `_describe` with dynamically fetched values.

### Subtasks

- [ ] cre.10.1: Implement agent name retrieval from index
- [ ] cre.10.2: Implement workspace path retrieval from index
- [ ] cre.10.3: Create dynamic completion data generator
- [ ] cre.10.4: Implement bash completion script with dynamic lookup
- [ ] cre.10.5: Implement zsh completion script with dynamic lookup
- [ ] cre.10.6: Implement fish completion script with dynamic lookup
- [ ] cre.10.7: Document shell completion setup
- [ ] cre.10.8: Add caching for completion data (avoid slow lookups)

---

## Implementation Order (REVISED)

Based on the review, many features already exist. Here's the updated plan:

### Already Done ✅
- **cre.1**: Quiet/Verbose modes (--quiet, --verbose exist)
- **cre.7**: Date range flags (--today, --week, --since, --until exist)

### Phase 1: Robot Mode Polish (cre.2)
- Add --robot-format (json, jsonl, compact)
- Add --robot-meta for extended metadata
- Track elapsed_ms and wildcard_fallback
- **Value**: Highest impact for AI agents

### Phase 2: Boolean Queries (cre.4)
- Implement boolean parser (AND, OR, NOT, phrases)
- **Value**: Unlocks power-user and agent workflows
- **Complexity**: Medium-high, but foundational

### Phase 3: Human CLI Formats (cre.3)
- Add --display (table, lines, markdown)
- **Value**: Users don't need TUI for quick searches

### Phase 4: Diagnostics (cre.9)
- Add `cass diagnose` with health checks
- **Value**: Self-service troubleshooting

### Phase 5: Context & Export (cre.8, cre.6)
- Add -C for context (like grep)
- Add `cass export` for markdown export
- **Value**: Better understanding and documentation

### Phase 6: Advanced Query (cre.5)
- Add field:value inline syntax
- **Value**: Ergonomic when combined with boolean operators

### Phase 7: Polish (cre.10)
- Dynamic shell completions
- **Value**: Nice DX improvement

---

## Testing Strategy

1. **Unit tests** for query parsing, formatters, health checks
2. **Integration tests** for CLI flag combinations
3. **Snapshot tests** for output format consistency
4. **Robot mode contract tests** ensuring JSON schema stability
5. **E2E tests** for diagnose command

---

## Documentation Updates Required

- [ ] Update --robot-help with --robot-format and --robot-meta
- [ ] Update README.md with boolean query syntax
- [ ] Add QUERY_SYNTAX.md with full documentation
- [ ] Update man page
- [ ] Add examples to --help output for new flags

