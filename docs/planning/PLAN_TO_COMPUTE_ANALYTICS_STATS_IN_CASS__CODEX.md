# Plan: Compute Comprehensive Analytics Stats (Tokens, Tools, Roles, Time) in CASS

> Prompt that kicked this off:
>
> ```
> vinchinzu (@vin6716): are you capturing any token metrics on any projects? to benchmark your own inteligence per MM token or $
> Jeffrey Emanuel (@doodlestein): I guess cass is grabbing all that stuff. I should build those analytics directly into cass.
> ```

## Executive Summary

`cass` already has the core of a **token + usage analytics pipeline** implemented (fact tables + rollups + ingest + rebuild). The remaining work is to make it coherent, queryable (robot-first CLI + shared query library), and richer across dimensions (models/tools/cost/plan attribution) without sacrificing performance.

The analytics system should answer questions like:

- Total tokens per **hour/day/week/month** (historical)
- Breakdowns across **agent types** (codex, claude_code, gemini, cursor, etc.)
- Breakdowns across **projects/workspaces** (and optionally sources/remotes)
- Averages like:
  - tokens per **human message**
  - tokens per **agent response**
  - tokens per **tool call**
  - tokens per **plan**
- Coverage/quality metrics (what % is real API usage vs estimation)

The core idea is:

1. Compute **per-message metrics once** at ingest time (or via backfill).
2. Store those metrics in a **narrow fact table** (no giant content blobs).
3. Maintain **hourly + daily rollup tables** via batched upserts for O(1) / O(#buckets) time-series queries.
4. Prefer **API-provided token usage** when available (e.g., Claude Code usage blocks), otherwise fall back to a deterministic estimate (existing `~chars/4` heuristic) while tracking quality explicitly.

No UI work in this plan; focus is compute + storage.

## Status Update (As Of 2026-02-06)

### Already Implemented In Code (DONE)
- Token extraction utilities exist in `src/connectors/mod.rs`: `extract_tokens_for_agent()` + per-agent extractors + `normalize_model()`.
- Track A analytics tables (schema v11) exist and are populated by live ingest:
  - `message_metrics` (fact table)
  - `usage_hourly`, `usage_daily` (rollups)
- Track B analytics tables (schema v10) exist and are populated by live ingest:
  - `token_usage` (per-message ledger)
  - `token_daily_stats` (daily rollups)
  - `model_pricing` (seeded)
  - conversation summary columns in `conversations` are updated from `token_usage`
- Live ingest plumbing populates both tracks in `SqliteStorage::insert_conversations_batched`.
- Rebuild/backfill exists: `SqliteStorage::rebuild_analytics()` rebuilds Track A.
- Tests already exist for schema/ingest/rebuild correctness (see `src/storage/sqlite.rs` test module).

### Remaining Work (OPEN)
- Coherency: rebuild/backfill must cover Track B too (or explicitly deprecate it) so drift cannot happen.
- Query surface: shared `crate::analytics` query library + robot-first CLI (`cass analytics … --json`).
- Dimensions: model-aware rollups for Track A; per-tool-name detail + rollups.
- Coverage improvements: Codex `token_count` wiring.
- Cost estimation: compute USD from `model_pricing` and expose coverage diagnostics.
- Validation/perf guardrails: fast invariants + drift detection + throughput budgets.

## 1. Existing Code + Why This Is Straightforward

Key facts from the current architecture:

- Connectors normalize conversations into `NormalizedConversation` / `NormalizedMessage` with:
  - `role` (user/assistant/agent/tool/system/unknown)
  - `content` (flattened text; tool-use blocks are flattened to `[Tool: X]` markers)
  - `extra` (raw per-agent JSON payload, often containing rich metadata)
- Indexer persists into SQLite (`conversations`, `messages`, etc.) and Tantivy.
- There is already a derived aggregation table `daily_stats` used for fast “sessions/messages/chars per day”.
- SQLite already stores some heavy JSON blobs in a compact binary form as well:
  - `conversations.metadata_bin` (MessagePack)
  - `messages.extra_bin` (MessagePack)
  This is ideal for **fast analytics backfills** without repeatedly parsing JSON.
- **Important**: `src/connectors/mod.rs` already contains token extraction utilities:
  - `extract_claude_code_tokens(extra)` parses Claude Code `message.usage`
  - `extract_codex_tokens(extra)` parses Codex `event_msg` `token_count` payload
  - `estimate_tokens_from_content(content, role)` does the deterministic `chars/4` fallback
  - `extract_tokens_for_agent(agent_slug, extra, content, role)` dispatches + preserves model/provider + tool counts

So we do not need to invent extraction; the main remaining engineering work is:

- Make Track A + Track B **coherent** under rebuild/backfill (no drift)
- Add a shared analytics query library + robot-first CLI surface
- Expand dimensions (models/tools) with rollups so queries stay O(#buckets)
- Fix ingestion gaps (Codex token_count events coverage)

Note: existing `daily_stats` buckets message counts by **conversation started_at** (because it is updated at conversation insert/append time).
For token analytics, we want buckets by **message timestamps** (created_at) so multi-day sessions attribute usage to the correct day/hour.
That is why this plan introduces new usage rollups instead of reusing `daily_stats`.

## 2. Definitions (Avoid “One Token Number” Confusion)

### 2.1 Two Kinds of Token Metrics

We should store and expose **two distinct token notions**:

1. **API usage tokens** (cost/compute relevant):
   - Comes from agent logs that include provider usage (e.g., Claude Code `message.usage`)
   - Has components like `input_tokens`, `output_tokens`, and sometimes cache tokens
   - Represents tokens consumed by the provider call, not just the visible message text

2. **Content tokens (estimated)** (message-size / corpus-volume relevant):
   - Deterministic estimate from message content using `chars/4` (already implemented)
   - Applies to every message, across all connectors, uniformly
   - Useful for “tokens per human message” in a consistent way

For “benchmark intelligence per MM token or $”, API usage tokens are what we ultimately want.
For per-message and per-role averages across heterogeneous agents, content-token estimates are often more stable.

### 2.2 Time Buckets

Use integer bucket ids for compactness and index efficiency:

- `hour_id`: hours since 2020-01-01 00:00:00 UTC
- `day_id`: days since 2020-01-01 00:00:00 UTC (already used by `daily_stats`)

Weeks/months can be computed from days quickly, but we can also materialize them later if needed.

### 2.3 Dimensions (What We Want to Slice By)

Minimum viable dimensions:

- `agent_slug` (claude_code, codex, cursor, gemini, aider, etc.)
- `workspace_id` (project path), with a sentinel for unknown
- `source_id` (local vs remote host/source), already present on conversations
- `role` (user/assistant/tool/system/other)

Optional expansion dims (phase 2+):

- `model` (raw model string) and normalized (provider/family/tier)
- `tool_name` (bash/read/etc) for tool-call analytics

## 3. Storage & Schema (SQLite)

### 3.1 Current Schema (Implemented)

The DB currently contains **two analytics tracks** (both populated by live ingest):

**Track A (schema v11): general message analytics**
- `message_metrics` (fact table; one row per message_id)
  - Dimensions: time buckets (hour/day), agent_slug, workspace_id, source_id, role
  - Metrics: content token estimate + API token components (when available) + tool_call_count + has_plan
- `usage_hourly`, `usage_daily` (rollups keyed by `(bucket, agent_slug, workspace_id, source_id)`)
  - Metrics: counts + content-est totals + API totals + coverage counts + plan_message_count

**Track B (schema v10): ledger + model/cost oriented**
- `token_usage` (per-message ledger keyed by message_id)
  - Adds: model_name/provider/service_tier, normalized model_family/model_tier, and a placeholder `estimated_cost_usd`
- `token_daily_stats` (daily rollups keyed by `(day_id, agent_slug, source_id, model_family)`)
- `model_pricing` (pattern table seeded with pricing rows)
- conversation token summary columns in `conversations` are updated from `token_usage`

### 3.2 Planned Schema Extensions (Next)

These additions keep queries fast without scanning raw message content.

**Tools (z9fse.6)**
- `tool_calls_detail`: per tool invocation (message_id + tool_name + buckets + dims)
  - Privacy constraint: do not store tool args by default.
- `tool_usage_hourly` / `tool_usage_daily`: rollups keyed by `(bucket, agent_slug, workspace_id, source_id, tool_name)`
  - Metrics: invocation_count, message_count_with_tool, api/content token totals attributed to tool-invoking messages, coverage counts.

**Models in Track A (z9fse.11)**
- Extend `message_metrics` with model fields:
  - `model_name`, `model_family`, `model_tier`, `provider`
- Add model rollups (do not change existing usage_* PKs):
  - `usage_models_daily` (and optionally hourly) keyed by `(bucket, agent_slug, workspace_id, source_id, model_family, model_tier)`

**Cost (z9fse.10)**
- Compute `token_usage.estimated_cost_usd` from `model_pricing` (effective-date aware + deterministic pattern selection).
- Sum into `token_daily_stats.estimated_cost_usd` and `conversations.estimated_cost_usd`.
- Optional (later): add `usd_est_total` columns to Track A rollups (usage_* / tool_* / model_* rollups) so tokens + USD can be queried through one contract.

### 3.3 Why Fact Tables + Rollups?

- Fact tables (`message_metrics`, `token_usage`, `tool_calls_detail`) allow:
  - rebuilding rollups without touching huge content blobs
  - correctness debugging (sum-of-facts == rollup invariants)
  - adding new rollups later without re-parsing agent logs
- Rollups (`usage_*`, `token_daily_stats`, tool/model rollups) enable:
  - time-series queries in O(#buckets) with tiny rows
  - top-N breakdowns without scanning millions of messages

### 3.4 Storage Efficiency Notes

- Keep analytics tables narrow: avoid JSON blobs.
- Prefer ints + small text dims; add surrogate keys only if a dimension explodes.
- Avoid row explosion by adding **separate** rollup tables (tools/models) instead of mutating existing primary keys.
- All derived tables must be rebuildable (see coherency plan in z9fse.13).

## 4. Ingestion Plan (Live, Incremental, Ultra Efficient)

**Status**: Track A + Track B analytics ingestion is already implemented in `SqliteStorage::insert_conversations_batched`.

Remaining ingest work is connector/dimension enrichment:
- Codex `token_count` wiring (better API coverage)
- per-tool-name extraction + rollups
- Track A model rollups
- cost estimation (USD) computation

### 4.1 Where To Hook In

All messages flow through SQLite insert points:

- `SqliteStorage::insert_conversation_tree` (new conversation)
- `SqliteStorage::append_messages` (existing conversation, new messages appended)
- `SqliteStorage::insert_conversations_batched` / `insert_conversation_in_tx_batched` (fast path used by indexer)

The batched path is the primary ingestion path and should remain the single place we extend analytics extraction/dims so we do not drift across codepaths.

### 4.2 Metric Extraction Per Inserted Message

When a message is inserted (we have `conv.agent_slug`, `conv.source_id`, `workspace_id`, `msg.role`, `msg.content`, `msg.extra_json`):

1. Determine `created_at_ms`:
   - `msg.created_at` if present
   - else fallback to `conv.started_at`
   - else fallback to `SqliteStorage::now_millis()` (last resort; mark as low quality if desired)

2. Compute bucket ids:
   - `day_id = SqliteStorage::day_id_from_millis(created_at_ms)`
   - `hour_id = (created_at_ms/1000 - EPOCH_2020_SECS) / 3600` (add helper `hour_id_from_millis`)

3. Compute content metrics:
   - `content_chars = msg.content.len()`
   - `content_tokens_est = content_chars / 4`

4. Extract API token usage (or fallback) using existing code:
   - `usage = connectors::extract_tokens_for_agent(&conv.agent_slug, &msg.extra_json, &msg.content, role_str)`
   - Persist the fields and also persist `usage.data_source` so we can compute coverage.

5. Heuristic flags:
   - `has_plan`: cheap heuristic (phase 1):
     - true if content contains a "Plan:" header, "## Plan", or starts with "Plan" and has numbered steps
     - intentionally simple; refine later

### 4.3 Batched Rollup Updates (Critical For Speed)

Do NOT upsert per message. Instead:

- While inserting a batch of messages, accumulate deltas in a `HashMap<(bucket_id, agent_slug, workspace_id, source_id), DeltaStruct>`
- At the end of the transaction, flush to `usage_hourly` and `usage_daily` via **multi-value INSERT with ON CONFLICT DO UPDATE**.

This is already the core pattern used in the code today:
- `AnalyticsRollupAggregator` (usage_hourly/usage_daily)
- `TokenStatsAggregator` (token_daily_stats)
- batch inserts for fact rows (`message_metrics`, `token_usage`)

### 4.4 “All” Rows vs Group-By

We have two options:

1. Store only exact dims and do `SUM(...) GROUP BY ...` over rollup table.
2. Store permutation rows like `agent_slug='all'`, `workspace_id=0 (all)`, `source_id='all'` for instant totals.

Recommendation:

- Start with exact dims only.
- If we find a real performance need, add permutation rows later using the same “expand” strategy as `daily_stats`.

Summing thousands of rollup rows is already cheap, and it avoids 4x/8x row count inflation.

## 5. Connector Gaps To Fix (For Real API Tokens + Tool Metrics)

### 5.1 Codex token_count events are currently skipped

`extract_codex_tokens` expects `event_msg.payload.type == token_count`, but `CodexConnector::scan` currently ignores those event types.

Plan:

- Update Codex connector parsing so token_count events are not discarded.
- Best approach: attach token_count data to the nearest preceding assistant response item in `NormalizedMessage.extra` under a `cass` namespace, e.g.:
  - `extra["cass"]["token_usage"]["output_tokens"] = ...`
  - then extend `extract_codex_tokens` to read from that location as well

This avoids polluting the searchable message stream with token-only synthetic messages.

Important operational note:
- Because token_count events have historically been dropped by the connector, old indexed Codex sessions in SQLite generally cannot be “fixed” by analytics rebuild alone.
- The backfill path is: re-index Codex sources (so assistant messages get updated `extra_*`), then rebuild analytics.

### 5.2 Tool calls and tool results (cross-agent)

We want “tokens per tool call” and “tool call counts by tool name”.

Phase 1:

- Use what we already extract for Claude Code:
  - tool_use blocks counted from `/message/content` where `type == tool_use`
- Store `tool_call_count` and `has_tool_calls` in `message_metrics`
- Roll up tool counts in hourly/daily tables
- Compute derived metrics:
  - `avg_api_tokens_per_tool_call = api_tokens_total / tool_call_count`

Phase 2+:

- Extend extractors for other connectors (Codex tool_call events, Cursor tool calls, etc.)
- Implement per-tool-name storage + rollups (z9fse.6): `tool_calls_detail` + `tool_usage_hourly`/`tool_usage_daily`, so tool queries are served from rollups (no full scans).

## 6. Backfill / Rebuild Strategy (Historical)

We need historical tokens across all existing indexed data.

**Status (today)**:
- Track A rebuild exists: `SqliteStorage::rebuild_analytics()` rebuilds `message_metrics` + `usage_*`.
- Track B rebuild is missing, but ingest writes `token_usage` + `token_daily_stats` and updates conversation summary columns.
- Coherency work (track-selectable rebuild + drift detection) is tracked in **z9fse.13**.

### 6.1 Rebuild Principles
- Analytics tables are **derived**; rebuild must never touch source session files.
- Rebuild is explicit (do not auto-run on startup).
- Rebuild must support **track selection** (A/B/all) and record meta so drift can be detected.

### 6.2 Track A Rebuild (Already Implemented)
- Clear `message_metrics`, `usage_hourly`, `usage_daily` in a transaction.
- Stream messages joined with dims:
  - messages + conversations (source_id) + agents (agent_slug) + workspaces (workspace_id)
  - Prefer decoding `messages.extra_bin` (MessagePack) when present.
- Compute per-message metrics via `extract_tokens_for_agent()` and insert `message_metrics` (batched).
- Populate `usage_*` rollups from the fact table.

### 6.3 Track B Rebuild (To Implement)
- Clear `token_usage` + `token_daily_stats` and reset conversation summary columns.
- Stream messages and rebuild `token_usage` deterministically (batched insert).
- Recompute `token_daily_stats` to match ingest semantics (prefer reusing `TokenStatsAggregator`).
- Update conversation summaries from `token_usage`.

### 6.4 Incremental Maintenance (Watch Mode)
After rebuild, live ingest keeps analytics up-to-date by inserting new fact rows and upserting rollups for new messages only.

No rescan required.

## 7. Query Surface (Robot-First)

We want other agents (and future dashboards) to consume analytics without re-implementing SQL.

Design rules:
- stdout = JSON data only; stderr = diagnostics
- buckets are UTC; weeks are ISO-8601 (Mon start)
- prefer rollups; if a slow path is used, it must be explicit in `_meta`

Implementation plan:
- Shared query layer: `crate::analytics` (z9fse.12)
- CLI contract: `cass analytics … --json` (z9fse.3)

Command tree (v1):
- `cass analytics status --json`
- `cass analytics tokens --json`
- `cass analytics tools --json`
- `cass analytics models --json`
- `cass analytics cost --json`
- `cass analytics rebuild --json`
- `cass analytics validate --json`

Common flags (where applicable):
- time window: `--since/--until`, `--days N`, `--today`, `--week`
- filters: `--agent`, `--workspace`, `--source`
- grouping: `--group-by hour|day|week|month`
- top-N: `--limit N` for breakdown-style commands

Coverage semantics (non-negotiable):
- report `api_coverage_message_count` and derived `api_coverage_pct`
- never present USD totals without pricing coverage signals (unknown != 0)
- always include `_meta` with elapsed_ms and query path (`rollup` vs `slow`)

## 8. “LOTS MORE” Metrics (Designed In, Implement Later)

With `message_metrics` as a fact table, adding new analytics becomes easy:

- Per-model tokens and cost:
  - add `dim_price(model_id, price_in, price_out, effective_from, ...)`
  - compute dollar estimates by joining rollups
- “Conversation structure” metrics:
  - average turns per session, tokens per session, tokens per hour of activity
- Tooling intensity metrics:
  - tool_calls per 1k tokens
  - average tool payload size (estimated)
- Planning metrics:
  - plan frequency and plan token share
- “Noise” metrics:
  - tokens in repeated tool acknowledgments (dedup by content hash)
- Speed metrics (if timestamps available):
  - tokens per minute of wall-clock session time
- Source comparisons:
  - local vs remote machines
  - which machine burns the most tokens

## 9. Testing Strategy

This work is analytics-critical: we need confidence that rollups match facts, rebuild is deterministic, and coverage/drift diagnostics are correct.

Already implemented tests (see `src/storage/sqlite.rs` test module):
- schema + migration checks for analytics tables/indexes
- ingest integration test that populates `message_metrics` + `usage_*` rollups
- plan heuristic unit tests for `has_plan`
- Track A rebuild integration test (clear + rebuild + verify)

Remaining additions (tracked primarily in z9fse.8 and z9fse.9):
1. Connector extraction unit tests
- Codex token_count attach + extraction path (z9fse.5)
- Tool-name extraction (z9fse.6)
- Model normalization edge cases (z9fse.11)

2. Storage + invariants integration tests
- Track B rebuild + coherency invariants (z9fse.13)
- Drift injection tests (delete/alter one analytics table) must be detected with actionable output (z9fse.9 / z9fse.3.5)
- Cost estimation arithmetic + pricing coverage rules (z9fse.10)

3. Robot/e2e shell scripts (tests/e2e/)
- Index deterministic fixture sessions and assert:
  - `cass analytics status --json` is sane and stable
  - `cass analytics tokens --group-by {hour,day,week,month} --json` totals are consistent across granularities
  - `cass analytics tools/models/cost --json` (once implemented) match fixture expectations and capture rich stderr diagnostics on failure

## 10. Implementation Plan (Aligned to Beads)

### DONE (already in code)
- `z9fse.1`: analytics schema v11 (`message_metrics`, `usage_hourly`, `usage_daily`)
- `z9fse.2`: live ingest analytics (fact + rollups populated on insert)
- `z9fse.4`: Track A rebuild/backfill (`rebuild_analytics()` rebuilds `message_metrics` + `usage_*`)
- `z9fse.7`: plan detection v1 (`has_plan` + `plan_message_count`)

### NEXT (core coherency + query surface)
- `z9fse.13`: make Track A + Track B coherent under rebuild/backfill (Track B rebuild + meta + drift signals)
- `z9fse.12`: shared analytics query library (bucket semantics, week/month aggregation, derived metrics)
- `z9fse.3.1` + `z9fse.3.2` + `z9fse.3.3`: CLI scaffolding + status + tokens
- `z9fse.8`: extend tests/e2e as CLI lands

### THEN (dimensions + coverage)
- `z9fse.5`: Codex token_count wiring (requires re-indexing Codex sources to backfill old sessions)
- `z9fse.11` + `z9fse.3.6`: model dimension + CLI models
- `z9fse.6` + `z9fse.3.9`: per-tool-name detail + rollups + CLI tools

### THEN (USD cost)
- `z9fse.10` + `z9fse.3.7`: cost estimation + CLI cost

### THEN (trust + docs)
- `z9fse.9` + `z9fse.3.5`: validator/perf guardrails + CLI validate
- `z9fse.3.8`: robot-docs for analytics

### LATER (plan analytics v2)
- `z9fse.14`: plan token attribution + heuristic refinement (plan token share, avg tokens per plan)

Out of scope for this plan: ftui analytics dashboards (`2noh9.4.18.*`). Those should consume `crate::analytics` so the numbers match CLI exactly.

## Open Questions

1. Codex `token_count` semantics: output-only, total, or something else? We need to confirm by inspecting real rollout logs.
2. Should we add a real tokenizer (BPE) for content tokens, or keep `chars/4` for now?
3. How aggressively should we denormalize dims into rollups (workspace_id + agent_slug + source_id)? Row count could grow; we should measure on a real corpus.
