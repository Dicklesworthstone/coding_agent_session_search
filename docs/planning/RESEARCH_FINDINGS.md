# RESEARCH FINDINGS: CASS (Coding Agent Session Search) - TOON Integration Analysis

**Researcher**: CrimsonForge (claude-code, claude-opus-4-5)
**Date**: 2026-01-23
**Bead**: bd-35m
**Tier**: 2 (Moderate Impact - Search results with long string values limit tabular compression)

---

## 1. Project Audit

### Architecture
CASS is a **Rust CLI tool** (edition 2024, v0.1.61) providing full-text search across AI coding agent sessions. It indexes conversations from 10+ agent types and provides structured output via multiple robot-friendly formats.

### Key Files
| File | Purpose |
|------|---------|
| `src/lib.rs` (~420KB) | Main CLI module: command definitions, output formatting, RobotFormat enum |
| `src/search/query.rs` | SearchHit struct, SearchResult, query execution |
| `Cargo.toml` | Binary "cass", serde + serde_json dependencies |

### Existing Output Formats
CASS already supports **4 robot output formats** via `--robot-format`:
1. **json** (default) - Pretty-printed JSON object (`serde_json::to_string_pretty`)
2. **jsonl** - Newline-delimited JSON (one hit per line + optional `_meta` header)
3. **compact** - Single-line JSON (`serde_json::to_string`)
4. **sessions** - Bare session paths (one per line, for chaining)

Plus **3 human-readable formats** via `--display`:
- table, lines, markdown

### Serialization Patterns
- **serde + serde_json** for all JSON output
- `SearchHit` struct with `#[derive(serde::Serialize)]`
- `filter_hit_fields()` → `apply_content_truncation()` → `clamp_hits_to_budget()` pipeline
- Output via `serde_json::to_string_pretty(&payload)` (Json) or `serde_json::to_string(&hit)` (Jsonl/Compact)

### Key Data Structures

```rust
// src/search/query.rs:758
pub struct SearchHit {
    pub title: String,
    pub snippet: String,
    pub content: String,
    pub score: f32,
    pub source_path: String,
    pub agent: String,
    pub workspace: String,
    pub workspace_original: Option<String>,  // skip_serializing_if None
    pub created_at: Option<i64>,
    pub line_number: Option<usize>,
    pub match_type: MatchType,
    pub source_id: String,    // default: "local"
    pub origin_kind: String,  // default: "local"
    pub origin_host: Option<String>,  // skip_serializing_if None
}
```

---

## 2. Output Analysis

### Sample Output Sizes (Actual Measurements)

| Command | Hits | JSON Bytes | TOON Bytes | Byte Savings | JSON Tokens | TOON Tokens | Token Savings |
|---------|------|-----------|------------|--------------|-------------|-------------|---------------|
| `cass health --json` | N/A | 634 | 489 | 22.9% | ~115 | ~97 | **15.7%** |
| `cass capabilities --json` | N/A | 859 | 573 | 33.3% | ~160 | ~136 | **15.0%** |
| `search --limit 3` | 3 | 1,840 | 1,123 | 38.9% | ~359 | ~263 | **26.7%** |
| `search --limit 20 --fields minimal` | 13 | 4,378 | N/A | N/A | ~913 | ~698 | **23.5%** |
| `search --limit 30 --fields summary` | 13 | 11,112 | N/A | N/A | ~2,307 | ~1,865 | **19.2%** |
| `search --limit 20 --max-content-length 100` | 13 | 10,291 | 9,035 | 12.2% | ~2,065 | ~1,879 | **9.0%** |

### Key Insight: Value-Heavy vs Key-Heavy Data

**Why savings are lower than UBS (9-27% vs UBS's 34-50%)**:

CASS search results contain **long string values** (file paths, content snippets, titles) that dominate the token count. TOON's primary savings mechanism is eliminating repeated key names in tabular data, but when values are 50-200 characters and keys are 5-15 characters, key elimination provides proportionally less savings.

Compare with UBS findings where values are short (`"critical"`, `3`, `"Use Number.isNaN(x)"`), making key repetition the dominant overhead.

### Tabular Data Candidates (MODERATE opportunity)

1. **`hits` array** (uniform SearchHit fields)
   - TOON: `hits[N]{agent,score,source_path,line_number,...}:` + CSV-like rows
   - Savings limited by long path/content values in each row
   - **Best with `--fields minimal`**: 23.5% savings (short values only)

2. **`features` array** (capabilities command - uniform strings)
   - TOON: Already compresses well: `features[22]: json_output,jsonl_output,...`
   - **15% savings** (already compact)

3. **`connectors` array** (capabilities - short strings)
   - TOON: `connectors[10]: codex,claude_code,gemini,...`
   - Minimal overhead in JSON anyway

### Key Folding Opportunities

- `state._meta.data_dir`, `state._meta.db_path`, `state._meta.timestamp`
- `state.database.conversations`, `state.database.exists`, `state.database.messages`
- `state.index.exists`, `state.index.fresh`, `state.index.stale`
- `state.pending.sessions`, `state.pending.watch_active`

### TOON Output Samples

**Search output (3 hits, --fields summary):**
```
count: 3
cursor: null
hits[3]{agent,content,created_at,line_number,match_type,origin_kind,score,snippet,source_id,source_path,title,workspace}:
  claude_code,commit changes to git repo,1768525988702,1,exact,local,55.169,commit changes to git repo,local,/home/ubuntu/.claude/projects/.../ce16a69a.jsonl,commit changes to git repo,/data/projects/beads_rust
  claude_code,"[Tool: Bash - Verify commit success]",1768532433520,10,exact,local,44.967,"[Tool: Bash - Verify commit success]",local,/home/ubuntu/.claude/projects/.../27545ba7.jsonl,commit changes to git repo,/data/projects/beads_rust
  claude_code,"[Tool: Bash - Verify commit succeeded]",1768526016823,11,exact,local,44.933,"[Tool: Bash - Verify commit succeeded]",local,/home/ubuntu/.claude/projects/.../ce16a69a.jsonl,commit changes to git repo,/data/projects/beads_rust
hits_clamped: false
limit: 5
max_tokens: null
offset: 0
query: git commit
request_id: null
total_matches: 3
```

**Capabilities output:**
```
crate_version: 0.1.61
api_version: 1
contract_version: "1"
features[22]: json_output,jsonl_output,robot_meta,time_filters,field_selection,...
connectors[10]: codex,claude_code,gemini,opencode,amp,cline,aider,cursor,chatgpt,pi_agent
limits:
  max_limit: 10000
  max_content_length: 0
  max_fields: 50
  max_agg_buckets: 10
```

---

## 3. Integration Assessment

### Complexity Rating: **Simple**

CASS is Rust with serde, and the format dispatch is a clean `match format {}` block.

### Recommended Approach: **Use toon_rust as a library crate**

`toon_rust` already exposes a Rust library. Prefer a direct crate dependency so CASS can encode/decode without spawning a subprocess:

```toml
# Cargo.toml
toon_rust = { path = "../toon_rust" }
```

```rust
RobotFormat::Toon => {
    let json_value = serde_json::to_value(&payload)?;
    let toon_str = toon_rust::encode(json_value, None);
    println!("{toon_str}");
}
```

This avoids process spawning, removes a binary dependency, and guarantees we use the toon_rust implementation.

### Fallback (Non-Rust only): toon_rust tru binary

If a non-Rust tool needs TOON, use the toon_rust `tru` binary explicitly (never the Node.js CLI).

### Key Integration Points

| File/Location | Change Required |
|---------------|-----------------|
| `src/lib.rs:853` | Add `Toon` variant to `RobotFormat` enum |
| `src/lib.rs:4302-4610` | Add `RobotFormat::Toon` match arm in `output_robot_results()` |
| `src/lib.rs:182` | Already handles `robot_format: Option<RobotFormat>` |
| `src/lib.rs:3624` | Format resolution logic (no changes needed) |
| `Cargo.toml` | Optionally add `toon_rust` path dependency (Pattern B only) |

### Dependencies
- Preferred: `toon_rust` crate + existing `serde`/`serde_json`
- Optional fallback (non-Rust): toon_rust `tru` binary (use `TOON_TRU_BIN` if PATH conflicts)

### Backwards Compatibility
- Zero risk: new `--robot-format toon` value, does not affect existing formats
- `--json` still defaults to `RobotFormat::Json`
- No breaking changes to any existing output

---

## 4. Token Savings Projections

| Usage Scenario | JSON Tokens | TOON Tokens | Savings |
|----------------|-------------|-------------|---------|
| Health check | ~115 | ~97 | ~16% |
| Capabilities query | ~160 | ~136 | ~15% |
| Small search (3 hits, full fields) | ~359 | ~263 | ~27% |
| Medium search (13 hits, minimal fields) | ~913 | ~698 | ~24% |
| Medium search (13 hits, summary fields) | ~2,307 | ~1,865 | ~19% |
| Large search (13 hits, full content) | ~2,065 | ~1,879 | ~9% |
| Projected: 50 hits, full fields | ~8,000+ | ~6,800+ | ~15% |
| Projected: 50 hits, minimal fields | ~3,500+ | ~2,700+ | ~23% |

**Key finding**: Token savings are inversely correlated with content field length. Best results with `--fields minimal` or `--max-content-length` limits.

**Recommendation**: When agents use `--robot-format toon`, they should also use `--fields minimal` or `--fields summary` to maximize compression benefits.

---

## 5. Special Considerations

### Language-Specific Notes
- CASS is **Rust** (edition 2024) with serde derives on all output types
- Use `toon_rust` as a library crate (already available)
- Avoid subprocesses; reserve the `tru` binary for non-Rust tools only
- `SearchHit` has `#[serde(skip_serializing_if = "Option::is_none")]` on optional fields, which TOON handles naturally (omitted fields = no row entry)

### TOON Effectiveness Factors
- **HIGH savings**: Commands with short, structured values (health, capabilities, status)
- **MODERATE savings**: Searches with `--fields minimal/summary` (short per-hit values)
- **LOW savings**: Searches with full content fields (long strings dominate token count)

### Implementation Order
1. Add `Toon` variant to `RobotFormat` enum
2. Add match arm calling `toon_rust::encode(...)`
3. Add `--robot-format toon` to CLI help/docs
4. Test with representative queries at various field/content limits
5. Consider adding `--fields` suggestions when `--robot-format toon` is used
6. Document that `--fields minimal` + `--robot-format toon` is optimal for token savings

### Risk Assessment
- **Low risk**: New format variant, no existing behavior changes
- **Dependency risk**: Adds `toon_rust` crate (path/git dependency)
- **Mitigation**: Fallback to JSON with warning if `toon_rust::encode` fails
- **Performance**: No subprocess overhead

---

## 6. Deliverables Checklist

- [x] RESEARCH_FINDINGS.md created (this file)
- [ ] Project-level beads created in .beads/ (see below)
- [ ] bd-308 (Integrate TOON into cass) updated with actual findings

---

## 7. Recommended Project-Level Beads

The following beads should be created for CASS TOON integration:

1. **cass-toon-enum**: Add `Toon` variant to `RobotFormat` enum in lib.rs:853
2. **cass-toon-output**: Implement TOON output in `output_robot_results()` match block
3. **cass-toon-fallback**: Graceful fallback to JSON on toon_rust encode errors
4. **cass-toon-test**: Add integration tests for `--robot-format toon` output
5. **cass-toon-docs**: Update CLI help text and README with TOON format option
6. **cass-toon-fields-hint**: Suggest `--fields minimal` when TOON format is used (optional UX improvement)

---

## 8. Comparison with UBS Integration

| Aspect | UBS | CASS |
|--------|-----|------|
| Language | Bash | Rust |
| Savings (typical) | 34-50% | 15-27% |
| Best case | 65 uniform findings | minimal-field searches |
| Integration method | toon_rust `tru` binary | toon_rust crate |
| Complexity | Simple | Simple |
| Tier | 1 (High Impact) | 2 (Moderate Impact) |
| Primary savings driver | Tabular findings array | Eliminating hit field keys |
| Limiting factor | None (short values) | Long string values in hits |
