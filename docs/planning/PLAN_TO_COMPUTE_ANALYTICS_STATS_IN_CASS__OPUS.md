# Plan: Comprehensive Token Analytics for CASS

> **Author:** WhiteSnow (Claude Opus 4.6)
> **Date:** 2026-02-05
> **Status:** Design proposal — awaiting approval
> **Origin:** Tweet from @vin6716 asking about token benchmarking per MM token / $

---

## Executive Summary

CASS already indexes conversations from 14+ coding agents into a unified SQLite database. The raw session data from many agents (especially Claude Code) contains **rich per-message token usage data** that is currently being stored in `messages.extra_json` but **never extracted or aggregated**. This plan adds a lightweight analytics layer that extracts, materializes, and pre-aggregates token metrics at indexing time — making any analytics query O(1) rather than requiring expensive full-table scans.

The design philosophy: **extract once at ingest, aggregate incrementally, query instantly.**

---

## Part 1: What Data Is Actually Available (Empirical Findings)

### Claude Code — GOLD MINE (Verified from live session data)

Every assistant message contains a full `usage` block:

```json
{
  "message": {
    "model": "claude-opus-4-6",
    "usage": {
      "input_tokens": 3,
      "output_tokens": 10,
      "cache_creation_input_tokens": 7997,
      "cache_read_input_tokens": 19152,
      "cache_creation": {
        "ephemeral_5m_input_tokens": 0,
        "ephemeral_1h_input_tokens": 7997
      },
      "service_tier": "standard",
      "inference_geo": "not_available"
    }
  },
  "requestId": "req_011CXq...",
  "timestamp": "2026-02-06T01:27:00.429Z"
}
```

User messages also contain: `thinkingMetadata.maxThinkingTokens`, `version`, `permissionMode`.

**In a single session (this one), 61 assistant messages consumed ~4.7M total tokens.**

### Codex — Token Events Available

```json
{"type":"event_msg","payload":{"type":"token_count","tokens":100}}
```

### Pi-Agent — Model & Thinking Available

Session header: `{"provider":"anthropic","modelId":"claude-3-opus","thinkingLevel":"medium"}`
Content blocks: `{type: "thinking", thinking: "..."}`, `{type: "toolCall", name: "..."}`

### Cursor — Model Names Available

`modelConfig.modelName` or bubble-level `modelType`/`model` field.

### Factory, OpenCode — Model Names Available

Both capture `message.model` or `modelID`.

### ChatGPT, Aider, Cline, Clawdbot, Vibe, Amp — Limited

No explicit token data. Must estimate from content length (~4 chars ≈ 1 token).

---

## Part 2: New Schema (Migration V10)

### 2.1 New Table: `token_usage` — Per-Message Token Ledger

This is the core accounting table. One row per API call that has token data.

```sql
CREATE TABLE IF NOT EXISTS token_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id INTEGER NOT NULL,   -- denormalized for fast aggregation
    agent_id INTEGER NOT NULL,          -- denormalized
    workspace_id INTEGER,               -- denormalized
    source_id TEXT NOT NULL DEFAULT 'local',

    -- Timing
    timestamp_ms INTEGER NOT NULL,      -- message created_at (ms since epoch)
    day_id INTEGER NOT NULL,            -- days since 2020-01-01 (matches daily_stats)

    -- Model identification
    model_name TEXT,                    -- e.g., "claude-opus-4-6", "gpt-4-turbo"
    model_family TEXT,                  -- e.g., "claude", "gpt", "gemini" (normalized)
    model_tier TEXT,                    -- e.g., "opus", "sonnet", "haiku", "4o", "flash"
    service_tier TEXT,                  -- e.g., "standard", "priority"
    provider TEXT,                      -- e.g., "anthropic", "openai", "google"

    -- Token counts (all nullable — not all agents provide all fields)
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,          -- cache_read_input_tokens
    cache_creation_tokens INTEGER,      -- cache_creation_input_tokens
    thinking_tokens INTEGER,            -- extended thinking budget used
    total_tokens INTEGER,               -- computed: input + output + cache_read + cache_creation

    -- Cost estimation (computed from model pricing table)
    estimated_cost_usd REAL,            -- NULL if model pricing unknown

    -- Message context
    role TEXT NOT NULL,                 -- 'user', 'assistant', 'tool', 'system'
    content_chars INTEGER NOT NULL,     -- character count of message content
    has_tool_calls INTEGER NOT NULL DEFAULT 0,  -- boolean: message contains tool_use blocks
    tool_call_count INTEGER NOT NULL DEFAULT 0, -- number of tool_use blocks

    -- Data quality
    data_source TEXT NOT NULL DEFAULT 'api',  -- 'api' (from usage block), 'estimated' (from char count)

    UNIQUE(message_id)
);

-- Hot-path indexes for analytics queries
CREATE INDEX IF NOT EXISTS idx_token_usage_day ON token_usage(day_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_conv ON token_usage(conversation_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_workspace ON token_usage(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp_ms);
```

### 2.2 New Table: `token_daily_stats` — Pre-Aggregated Daily Rollups

Follows the exact same pattern as existing `daily_stats` table, but for token metrics.

```sql
CREATE TABLE IF NOT EXISTS token_daily_stats (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,           -- 'all' for totals, or specific agent slug
    source_id TEXT NOT NULL DEFAULT 'all',
    model_family TEXT NOT NULL DEFAULT 'all', -- 'all', 'claude', 'gpt', 'gemini', etc.

    -- Counters
    api_call_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_message_count INTEGER NOT NULL DEFAULT 0,

    -- Token sums
    total_input_tokens INTEGER NOT NULL DEFAULT 0,
    total_output_tokens INTEGER NOT NULL DEFAULT 0,
    total_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    total_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    total_thinking_tokens INTEGER NOT NULL DEFAULT 0,
    grand_total_tokens INTEGER NOT NULL DEFAULT 0,  -- sum of all above

    -- Content metrics
    total_content_chars INTEGER NOT NULL DEFAULT 0,
    total_tool_calls INTEGER NOT NULL DEFAULT 0,

    -- Cost
    estimated_cost_usd REAL NOT NULL DEFAULT 0.0,

    -- Session-level metrics (for averages)
    session_count INTEGER NOT NULL DEFAULT 0,

    -- Bookkeeping
    last_updated INTEGER NOT NULL,

    PRIMARY KEY (day_id, agent_slug, source_id, model_family)
);

CREATE INDEX IF NOT EXISTS idx_token_daily_stats_agent ON token_daily_stats(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_token_daily_stats_model ON token_daily_stats(model_family, day_id);
```

### 2.3 New Table: `model_pricing` — Cost Lookup Table

```sql
CREATE TABLE IF NOT EXISTS model_pricing (
    model_pattern TEXT NOT NULL,         -- regex or glob pattern (e.g., "claude-opus-4*")
    provider TEXT NOT NULL,
    input_cost_per_mtok REAL NOT NULL,   -- $ per million input tokens
    output_cost_per_mtok REAL NOT NULL,  -- $ per million output tokens
    cache_read_cost_per_mtok REAL,       -- $ per million cache read tokens
    cache_creation_cost_per_mtok REAL,   -- $ per million cache creation tokens
    effective_date TEXT NOT NULL,         -- ISO-8601 date when this pricing took effect
    PRIMARY KEY (model_pattern, effective_date)
);

-- Seed with current pricing (as of 2026-02)
INSERT OR IGNORE INTO model_pricing VALUES
    ('claude-opus-4%', 'anthropic', 15.0, 75.0, 1.5, 18.75, '2025-10-01'),
    ('claude-sonnet-4%', 'anthropic', 3.0, 15.0, 0.3, 3.75, '2025-10-01'),
    ('claude-haiku-4%', 'anthropic', 0.80, 4.0, 0.08, 1.0, '2025-10-01'),
    ('gpt-4o%', 'openai', 2.50, 10.0, NULL, NULL, '2025-01-01'),
    ('gpt-4-turbo%', 'openai', 10.0, 30.0, NULL, NULL, '2024-04-01'),
    ('gpt-4.1%', 'openai', 2.0, 8.0, NULL, NULL, '2025-04-01'),
    ('o3%', 'openai', 2.0, 8.0, NULL, NULL, '2025-04-01'),
    ('o4-mini%', 'openai', 1.10, 4.40, NULL, NULL, '2025-04-01'),
    ('gemini-2%flash%', 'google', 0.075, 0.30, NULL, NULL, '2025-01-01'),
    ('gemini-2%pro%', 'google', 1.25, 10.0, NULL, NULL, '2025-01-01');
```

### 2.4 Extend Existing `conversations` Table

Add computed summary columns for fast per-conversation queries:

```sql
-- Migration V10 additions to conversations table
ALTER TABLE conversations ADD COLUMN total_input_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN total_output_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN total_cache_read_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN total_cache_creation_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN grand_total_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN estimated_cost_usd REAL;
ALTER TABLE conversations ADD COLUMN primary_model TEXT;    -- most-used model in conversation
ALTER TABLE conversations ADD COLUMN api_call_count INTEGER;
ALTER TABLE conversations ADD COLUMN tool_call_count INTEGER;
ALTER TABLE conversations ADD COLUMN user_message_count INTEGER;
ALTER TABLE conversations ADD COLUMN assistant_message_count INTEGER;
```

---

## Part 3: Token Extraction at Ingest Time

### 3.1 `TokenExtractor` — Per-Connector Token Parsing

Each connector already stores the raw JSON in `NormalizedMessage.extra`. We add a `TokenExtractor` trait that each connector can implement:

```rust
/// Extracted token usage from a single message's raw data.
#[derive(Debug, Clone, Default)]
pub struct ExtractedTokenUsage {
    pub model_name: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub service_tier: Option<String>,
    pub has_tool_calls: bool,
    pub tool_call_count: u32,
    pub data_source: TokenDataSource,
}

#[derive(Debug, Clone, Default)]
pub enum TokenDataSource {
    Api,         // Actual token counts from API response
    #[default]
    Estimated,   // Estimated from content length
}
```

### 3.2 Connector-Specific Extractors

**Claude Code** (highest fidelity):
```rust
fn extract_claude_code_tokens(extra: &Value) -> ExtractedTokenUsage {
    let usage = extra.pointer("/message/usage");
    let model = extra.pointer("/message/model").and_then(|v| v.as_str());
    // Extract: input_tokens, output_tokens, cache_read_input_tokens,
    //          cache_creation_input_tokens, service_tier
    // Count tool_use blocks in message.content array
}
```

**Codex:**
```rust
fn extract_codex_tokens(extra: &Value) -> ExtractedTokenUsage {
    // Check for event_msg with payload.type == "token_count"
    // Extract payload.tokens
}
```

**All others (fallback):**
```rust
fn estimate_tokens_from_content(content: &str, role: &str) -> ExtractedTokenUsage {
    // Heuristic: ~4 chars per token (conservative)
    // For assistant messages: output_tokens = content_len / 4
    // For user messages: input_tokens = content_len / 4
    ExtractedTokenUsage {
        data_source: TokenDataSource::Estimated,
        ..
    }
}
```

### 3.3 Model Name Normalization

```rust
/// Normalize raw model strings into (family, tier, provider) tuples.
fn normalize_model(raw: &str) -> (String, String, String) {
    // "claude-opus-4-6"        → ("claude", "opus", "anthropic")
    // "claude-sonnet-4-5-20250929" → ("claude", "sonnet", "anthropic")
    // "claude-haiku-4-5-20251001"  → ("claude", "haiku", "anthropic")
    // "gpt-4o"                 → ("gpt", "4o", "openai")
    // "gpt-4-turbo"            → ("gpt", "4-turbo", "openai")
    // "gemini-2.0-flash"       → ("gemini", "flash", "google")
    // "o3"                     → ("gpt", "o3", "openai")
    // Unknown                  → ("unknown", raw, "unknown")
}
```

---

## Part 4: Aggregation Pipeline

### 4.1 `TokenStatsAggregator` — Mirrors Existing `StatsAggregator`

Follow the exact same proven pattern as `StatsAggregator` (storage/sqlite.rs:959):

```rust
#[derive(Debug, Default)]
pub struct TokenStatsAggregator {
    // Key: (day_id, agent_slug, source_id, model_family)
    raw: HashMap<(i64, String, String, String), TokenStatsDelta>,
}

#[derive(Debug, Default)]
pub struct TokenStatsDelta {
    pub api_call_count: i64,
    pub user_message_count: i64,
    pub assistant_message_count: i64,
    pub tool_message_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_creation_tokens: i64,
    pub total_thinking_tokens: i64,
    pub grand_total_tokens: i64,
    pub total_content_chars: i64,
    pub total_tool_calls: i64,
    pub estimated_cost_usd: f64,
    pub session_count: i64,
}
```

During batch ingestion, for each message with token data:
1. Extract tokens via `TokenExtractor`
2. Record into `TokenStatsAggregator`
3. At commit time, call `expand()` to generate the 5 permutation keys:
   - `(day, specific_agent, specific_source, specific_model)` — raw entry
   - `(day, "all", specific_source, specific_model)` — all agents
   - `(day, specific_agent, "all", specific_model)` — all sources
   - `(day, specific_agent, specific_source, "all")` — all models
   - `(day, "all", "all", "all")` — global total
4. Flush via multi-value `INSERT...ON CONFLICT DO UPDATE`

### 4.2 Conversation-Level Summaries

After all messages for a conversation are ingested, compute and store per-conversation totals:

```sql
UPDATE conversations SET
    total_input_tokens = (SELECT SUM(input_tokens) FROM token_usage WHERE conversation_id = ?),
    total_output_tokens = (SELECT SUM(output_tokens) FROM token_usage WHERE conversation_id = ?),
    -- ... etc
    primary_model = (SELECT model_name FROM token_usage WHERE conversation_id = ?
                     GROUP BY model_name ORDER BY COUNT(*) DESC LIMIT 1),
    api_call_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?
                      AND data_source = 'api')
WHERE id = ?;
```

---

## Part 5: The Analytics Catalog — What We Can Compute

### 5.1 Time-Series Metrics (from `token_daily_stats`)

All of these are O(1) lookups against the materialized table:

| Metric | Query Pattern |
|--------|--------------|
| **Tokens per hour/day/week/month** | `SUM(grand_total_tokens) WHERE day_id BETWEEN ? AND ?` |
| **Input vs output ratio over time** | `SUM(total_input_tokens) / SUM(total_output_tokens)` |
| **Cache hit rate over time** | `SUM(cache_read_tokens) / (SUM(cache_read_tokens) + SUM(total_input_tokens))` |
| **Cost per day/week/month** | `SUM(estimated_cost_usd) WHERE day_id BETWEEN ? AND ?` |
| **Sessions per day** | `SUM(session_count) WHERE day_id BETWEEN ? AND ?` |
| **Messages per session (avg)** | `SUM(assistant_message_count) / SUM(session_count)` |
| **Tokens per session (avg)** | `SUM(grand_total_tokens) / SUM(session_count)` |
| **Tool calls per session (avg)** | `SUM(total_tool_calls) / SUM(session_count)` |

### 5.2 Cross-Agent Comparisons (from `token_daily_stats`)

| Metric | Query Pattern |
|--------|--------------|
| **Tokens per agent type** | `GROUP BY agent_slug WHERE model_family = 'all'` |
| **Cost per agent type** | `SUM(estimated_cost_usd) GROUP BY agent_slug` |
| **Efficiency: tokens per char of output** | `SUM(grand_total_tokens) / SUM(total_content_chars)` |
| **Agent usage distribution** | `SUM(api_call_count) GROUP BY agent_slug` |

### 5.3 Model-Level Analytics (from `token_daily_stats`)

| Metric | Query Pattern |
|--------|--------------|
| **Tokens per model family** | `GROUP BY model_family WHERE agent_slug = 'all'` |
| **Model tier distribution** | `SUM(api_call_count) GROUP BY model_family` |
| **Cost per model** | `SUM(estimated_cost_usd) GROUP BY model_family` |
| **Model migration trends** | `SUM(api_call_count) GROUP BY model_family, day_id` |

### 5.4 Per-Project Analytics (from `token_usage` + joins)

| Metric | Query Pattern |
|--------|--------------|
| **Total tokens per workspace** | `SUM(total_tokens) GROUP BY workspace_id` |
| **Cost per project** | `SUM(estimated_cost_usd) GROUP BY workspace_id` |
| **Most expensive projects** | `ORDER BY SUM(estimated_cost_usd) DESC LIMIT 10` |
| **Project activity heatmap** | `COUNT(*) GROUP BY workspace_id, day_id` |

### 5.5 Per-Message Analytics (from `token_usage`)

| Metric | Description |
|--------|-------------|
| **Avg tokens per human message** | `AVG(input_tokens) WHERE role = 'user'` |
| **Avg tokens per agent response** | `AVG(output_tokens) WHERE role = 'assistant'` |
| **Avg tokens per tool call** | `AVG(total_tokens) WHERE has_tool_calls = 1` |
| **Token distribution (p50/p90/p99)** | `NTILE(100) OVER (ORDER BY total_tokens)` |
| **Largest single responses** | `ORDER BY output_tokens DESC LIMIT 10` |
| **Most token-expensive conversations** | `ORDER BY grand_total_tokens DESC LIMIT 10` |

### 5.6 Cache Efficiency Analytics (Claude Code specific)

| Metric | Description |
|--------|-------------|
| **Cache hit rate** | `cache_read / (cache_read + input_tokens)` |
| **Cache savings (estimated $)** | `cache_read_tokens * (full_price - cache_price) / 1M` |
| **Cache creation overhead** | `cache_creation_tokens * creation_price / 1M` |
| **Net cache benefit** | `savings - creation_overhead` |
| **Cache hit rate trend** | Over time: are we getting better at caching? |

### 5.7 Productivity & Intelligence Metrics

| Metric | Description |
|--------|-------------|
| **Intelligence per $** | `output_chars / estimated_cost_usd` (useful output per dollar) |
| **Intelligence per MTok** | `output_chars / (grand_total_tokens / 1M)` |
| **Tokens per line of code changed** | If we can extract code diffs from tool calls |
| **Thinking efficiency** | `output_tokens / (input_tokens + thinking_tokens)` |
| **Session depth** | `message_count / session_duration_hours` |
| **Turn efficiency** | `useful_output_chars / total_turns` |

### 5.8 Cross-Machine Analytics (from `source_id` dimension)

| Metric | Description |
|--------|-------------|
| **Tokens per machine** | `GROUP BY source_id` |
| **Cost per machine** | `SUM(estimated_cost_usd) GROUP BY source_id` |
| **Machine utilization patterns** | Active hours heatmap per source |

---

## Part 6: Implementation Strategy

### Phase 1: Schema + Extraction (Estimated: 1 session)

1. Add Migration V10 with all new tables/columns
2. Implement `TokenExtractor` trait + Claude Code extractor (highest value)
3. Implement model name normalization
4. Implement fallback content-length estimator
5. Wire extraction into `insert_conversation_in_tx_batched()`

### Phase 2: Aggregation (Estimated: 1 session)

1. Implement `TokenStatsAggregator` (follow `StatsAggregator` pattern exactly)
2. Wire into batch ingestion pipeline alongside existing `StatsAggregator`
3. Implement `rebuild_token_daily_stats()` for full rebuild from `token_usage`
4. Add conversation-level summary computation

### Phase 3: Remaining Connectors (Estimated: 1 session)

1. Codex token extractor (event_msg parsing)
2. Pi-Agent token extractor (model tracking, thinking detection)
3. Cursor token extractor (model name extraction)
4. Factory/OpenCode token extractors
5. Fallback estimator for Aider, Cline, ChatGPT, Clawdbot, Vibe, Amp

### Phase 4: Robot-Mode Query API (Estimated: 1 session)

1. Add `cass analytics` subcommand with `--robot` output
2. Expose pre-computed time-series, cross-agent, model-level queries
3. Add `cass analytics --summary` for single-shot overview
4. Wire into existing `cass stats` command for backward compatibility

### Phase 5: Backfill + Cost Engine (Estimated: 1 session)

1. One-time backfill: re-read all `extra_json`/`extra_bin` from messages table
2. Seed `model_pricing` table
3. Implement cost computation engine
4. Run full `rebuild_token_daily_stats()`

---

## Part 7: Efficiency Guarantees

### At Ingest Time
- Token extraction adds **~1μs per message** (JSON pointer lookup + integer extraction)
- `TokenStatsAggregator` is in-memory HashMap — zero DB overhead until flush
- Flush is a single multi-value INSERT (same as existing `StatsAggregator`)
- Net overhead: **< 1% of total indexing time**

### At Query Time
- All analytics queries hit **materialized tables** with covering indexes
- `token_daily_stats` has 4-column composite PK + indexes = O(1) range scans
- No JOINs needed for time-series queries (everything denormalized)
- Conversation-level queries use denormalized columns (no subquery needed)

### Storage Overhead
- `token_usage`: ~100 bytes per message × estimated 500K messages = ~50MB
- `token_daily_stats`: ~200 bytes per row × estimated 10K rows = ~2MB
- `model_pricing`: < 1KB
- Conversation column additions: ~80 bytes per conversation × 20K convos = ~1.6MB
- **Total: ~54MB** (negligible compared to existing DB + FTS index)

### Memory at Runtime
- `TokenStatsAggregator` holds at most ~10K entries in memory during batch ingest
- Each entry: ~200 bytes → ~2MB peak memory during indexing
- Freed immediately after flush

---

## Part 8: Code Organization

All new code goes into **existing files** (per AGENTS.md no-file-proliferation rule):

| Component | File | Rationale |
|-----------|------|-----------|
| `token_usage` table schema | `src/storage/sqlite.rs` | Migration V10, next to existing migrations |
| `TokenExtractor` trait + implementations | `src/connectors/mod.rs` | Alongside existing `Connector` trait |
| Claude Code extractor | `src/connectors/claude_code.rs` | Connector-specific logic stays in connector |
| Codex extractor | `src/connectors/codex.rs` | Same |
| Model normalization | `src/connectors/mod.rs` | Shared utility for all connectors |
| `TokenStatsAggregator` | `src/storage/sqlite.rs` | Next to existing `StatsAggregator` |
| Conversation summaries | `src/storage/sqlite.rs` | Part of insert pipeline |
| Robot-mode analytics API | `src/lib.rs` | Where other subcommands are defined |
| Analytics page data | `src/pages/analytics.rs` | Extends existing analytics bundle |
| Cost computation | `src/storage/sqlite.rs` | Utility function near `model_pricing` table |

---

## Part 9: Key Design Decisions

### Why a separate `token_usage` table instead of adding columns to `messages`?

1. **Not all messages have token data** — many agents don't provide it. A separate table avoids NULL-heavy columns.
2. **Denormalization for speed** — `token_usage` includes `agent_id`, `workspace_id`, `day_id` so analytics queries never need JOINs.
3. **Clean separation of concerns** — message content is for search; token data is for analytics.
4. **Backfill-friendly** — can be rebuilt from `messages.extra_json` without touching the messages table.

### Why materialized daily stats instead of on-the-fly aggregation?

1. **O(1) vs O(N)** — with 500K+ messages, aggregation queries would take seconds. Materialized: < 1ms.
2. **Proven pattern** — the existing `daily_stats` table uses this exact approach and it works.
3. **Incremental updates** — only new data needs to be aggregated, not the entire history.

### Why denormalize model_family into the aggregation key?

1. **Model comparison is the #1 analytics use case** — "how much did Opus cost vs Sonnet?"
2. **Without it, every model query requires a JOIN or subquery** on `token_usage`.
3. **Cardinality is low** — maybe 10 model families × 365 days × 5 agents × 3 sources = ~55K rows/year. Tiny.

### Why estimate tokens for agents that don't provide them?

1. **Completeness** — analytics dashboards shouldn't have blank rows for Aider/Cline
2. **Rough is better than nothing** — `~4 chars/token` is a well-known heuristic
3. **Clearly flagged** — `data_source = 'estimated'` lets consumers filter or weight accordingly

### Why store model pricing in the DB rather than hardcode?

1. **Prices change** — new models launch, prices drop
2. **User-configurable** — power users can add custom model pricing
3. **Historical accuracy** — effective_date allows correct cost computation for past data
4. **Pattern matching** — `model_pattern` supports wildcards for model family grouping

---

## Part 10: Example Outputs

### `cass analytics --summary --robot`

```json
{
  "period": "all_time",
  "totals": {
    "conversations": 12847,
    "messages": 487231,
    "api_calls_with_token_data": 198432,
    "grand_total_tokens": 8_432_198_765,
    "total_input_tokens": 1_234_567_890,
    "total_output_tokens": 987_654_321,
    "total_cache_read_tokens": 5_432_198_765,
    "total_cache_creation_tokens": 777_777_789,
    "estimated_total_cost_usd": 1247.83
  },
  "by_agent": {
    "claude_code": { "tokens": 6_100_000_000, "cost_usd": 987.50, "sessions": 8432 },
    "codex": { "tokens": 1_200_000_000, "cost_usd": 145.20, "sessions": 2100 },
    "cursor": { "tokens": 800_000_000, "cost_usd": 89.30, "sessions": 1500 },
    "gemini": { "tokens": 332_198_765, "cost_usd": 25.83, "sessions": 815 }
  },
  "by_model_family": {
    "claude": { "tokens": 6_500_000_000, "cost_usd": 1050.00, "calls": 150000 },
    "gpt": { "tokens": 1_500_000_000, "cost_usd": 150.00, "calls": 35000 },
    "gemini": { "tokens": 432_198_765, "cost_usd": 47.83, "calls": 13432 }
  },
  "averages": {
    "tokens_per_session": 65612,
    "tokens_per_human_message": 1234,
    "tokens_per_agent_response": 4567,
    "tokens_per_tool_call": 890,
    "cost_per_session_usd": 0.097,
    "cache_hit_rate": 0.73,
    "messages_per_session": 37.9
  },
  "trends_30d": {
    "daily_avg_tokens": 28_107_329,
    "daily_avg_cost_usd": 41.59,
    "daily_avg_sessions": 428,
    "token_growth_rate_pct": 12.3
  }
}
```

### `cass analytics --by-model --days 7 --robot`

```json
{
  "period": "7d",
  "models": [
    {
      "model": "claude-opus-4-6",
      "family": "claude", "tier": "opus", "provider": "anthropic",
      "total_tokens": 2_100_000_000,
      "input_tokens": 300_000_000,
      "output_tokens": 250_000_000,
      "cache_read_tokens": 1_400_000_000,
      "cache_creation_tokens": 150_000_000,
      "estimated_cost_usd": 340.50,
      "api_calls": 15000,
      "avg_tokens_per_call": 140000,
      "cache_hit_rate": 0.82
    },
    {
      "model": "claude-haiku-4-5",
      "family": "claude", "tier": "haiku", "provider": "anthropic",
      "total_tokens": 500_000_000,
      "estimated_cost_usd": 12.30,
      "api_calls": 8000,
      "avg_tokens_per_call": 62500,
      "cache_hit_rate": 0.65
    }
  ]
}
```

---

## Part 11: Future Extensions (Not In Scope Now)

These are explicitly **out of scope** for this plan but would be natural follow-ons:

1. **TUI Dashboard** — Sparkline charts, bar graphs in the terminal
2. **Budget Alerts** — "You've spent $X this week" notifications
3. **Token Budget Mode** — Set spending limits per project/day
4. **Export to CSV/Parquet** — For external analysis tools
5. **Comparative Intelligence Score** — Benchmarking output quality per token across models
6. **Real-time Streaming** — Watch token usage live as sessions progress
7. **API Rate Monitoring** — Track requests/minute against rate limits
8. **Multi-machine Cost Allocation** — Per-machine cost reports
9. **HTML Pages Integration** — Token analytics in the self-hosted web dashboard

---

## Summary

This plan adds **comprehensive token analytics** to CASS by:

1. **Extracting** token usage data that's already sitting unused in `messages.extra_json`
2. **Storing** it in a dedicated `token_usage` table with full denormalization
3. **Pre-aggregating** into `token_daily_stats` for instant dashboard queries
4. **Computing costs** using a configurable model pricing table
5. **Exposing** via `cass analytics --robot` for machine consumption

The design is:
- **Zero overhead at query time** (materialized tables)
- **Negligible overhead at ingest time** (< 1% of indexing time)
- **~54MB storage** for 500K messages
- **Incrementally updatable** (no full rebuilds needed)
- **Backward compatible** (new tables/columns, no breaking changes)
- **Data-quality aware** (`data_source` distinguishes API data from estimates)

All code goes into existing files. No new files needed.
