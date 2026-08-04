# Deterministic Answer-Pack Contract

**Bead:** `coding_agent_session_search-uuwye.1`
**Status:** Contract for implementation beads
**Date:** 2026-05-08

This document defines the first implementation contract for `cass pack`: a
robot-first command that turns existing indexed session evidence into a compact,
cited handoff artifact for agents and humans. The feature is deterministic and
extractive. It does not call an LLM, does not mutate source logs, and does not
repair indexes except through already-blessed search health and refresh paths.

## Goals

- Produce a token-budgeted handoff bundle for a query, issue, review target, or
  implementation task.
- Preserve enough provenance for every extracted fact to be traced back to a
  session path and line span.
- Fail open to lexical evidence when semantic assets are absent, matching the
  search asset contract.
- Emit stable robot-mode schemas across JSON, compact JSON, JSONL, and TOON.
- Emit Markdown for human handoff notes without weakening the robot contract.

## Non-Goals

- No external LLM summarization or rewriting.
- No automatic model download.
- No mutation of agent source histories.
- No deletion or cleanup of derived assets.
- No new `rusqlite` usage.
- No bare interactive TUI path.
- No hidden `doctor --fix`, index repair, or quarantine garbage collection.

## Command Surface

The command name is:

```bash
cass pack <query>
```

Machine output uses the existing global robot format contract:

```bash
cass pack "storage open recovery" --json
cass pack "storage open recovery" --robot-format compact
cass pack "storage open recovery" --robot-format jsonl
cass pack "storage open recovery" --robot-format toon
```

Markdown output is explicit human display output:

```bash
cass pack "storage open recovery" --display markdown
```

`--robot-format sessions` is not supported for `pack`; it returns
`err.kind="pack-unsupported-format"` because a pack is not a path stream.

## Input Flags

The command reuses search filters where possible:

| Flag | Type | Default | Contract |
|------|------|---------|----------|
| `<query>` | string | required | Non-empty natural-language or keyword query. |
| `--agent <slug>` | repeatable string | empty | Restrict evidence to matching agent slugs. |
| `--workspace <path>` | repeatable path string | empty | Restrict evidence to matching workspace paths. |
| `--source <id>` | string | `all` | `local`, `remote`, `all`, or a configured source id. |
| `--sessions-from <path|->` | string | none | Read candidate session paths, one per line, with `-` for stdin. |
| `--days <n>` | integer | none | Restrict evidence to the last `n` days. |
| `--today` | bool | false | Restrict evidence to the current local day. |
| `--yesterday` | bool | false | Restrict evidence to the previous local day. |
| `--week` | bool | false | Restrict evidence to the last 7 days. |
| `--since <time>` | string | none | Same syntax as search: ISO, keyword, or relative offset. |
| `--until <time>` | string | none | Same syntax as search: ISO, keyword, or relative offset. |
| `--mode <mode>` | enum | `hybrid` | `hybrid`, `lexical`, or `semantic`; hybrid must fail open to lexical. |
| `--approximate` | bool | false | Passed through to semantic/hybrid ANN search when available. |
| `--refresh` | bool | false | Runs the existing safe incremental search refresh before packing. |
| `--timeout <ms>` | integer | none | Bounded execution; partial packs must be marked partial. |
| `--request-id <id>` | string | none | Echoed in `_meta.request_id`. |
| `--data-dir <path>` | path | platform default | Same data directory semantics as search. |
| `--db <path>` | path | platform default | Same database override semantics as global CLI. |

Pack-specific flags:

| Flag | Type | Default | Range / Values |
|------|------|---------|----------------|
| `--max-tokens <n>` | integer | `12000` | Minimum `1024`, maximum `200000`. |
| `--max-sessions <n>` | integer | `8` | Minimum `1`, maximum `64`. |
| `--max-evidence <n>` | integer | `24` | Minimum `1`, maximum `256`. |
| `--context-lines <n>` | integer | `3` | Minimum `0`, maximum `40`. |
| `--max-excerpt-chars <n>` | integer | budget-derived, cap `1600` | Minimum `80`, maximum `8000`. |
| `--field-mask <preset|list>` | string | `standard` | Presets and explicit fields below. |
| `--freshness-policy <policy>` | enum | `prefer-recent` | `prefer-recent`, `strict`, `allow-stale`. |
| `--freshness-window <duration>` | string | `30d` | Relative duration: `h`, `d`, `w`, or `m`. |
| `--redaction <policy>` | enum | `strict` | `strict`, `balanced`, `off`. |
| `--include-skill-content` | bool | false | Includes skill payload excerpts only when explicitly requested. |
| `--require-evidence` | bool | false | Empty packs become an error instead of success with no evidence. |
| `--explain-selection` | bool | false | Includes per-candidate score components and rejection reasons. |

`--redaction off` is allowed only for local operator workflows. Robot output must
still set `privacy.redaction_policy="off"` and `privacy.sensitive_output=true`.

## Field Masks

`--field-mask standard` includes:

- `schema_version`
- `query`
- `_meta`
- `limits`
- `realized`
- `health`
- `freshness`
- `pack`
- `evidence`
- `omitted`
- `privacy`
- `warnings`

`--field-mask minimal` includes:

- `schema_version`
- `query.text`
- `realized.search_mode`
- `pack.answer_outline`
- `evidence[].citation`
- `evidence[].excerpt`
- `omitted.count`
- `privacy.redaction_applied`

`--field-mask full` includes every defined response field, including
`selection_debug` when `--explain-selection` is set.

Explicit masks are comma-separated top-level or dotted field names. Unknown
fields are ignored with a warning in `_meta.warnings[]`; they do not change the
exit code.

## JSON Response Schema

All object keys are stable and snake_case.

```json
{
  "schema_version": "cass.pack.v1",
  "query": {
    "text": "storage open recovery",
    "normalized": "storage open recovery",
    "filters": {}
  },
  "_meta": {
    "request_id": "optional-client-id",
    "generated_at_ms": 1778219200000,
    "elapsed_ms": 42,
    "partial": false,
    "format": "json",
    "warnings": []
  },
  "limits": {
    "max_tokens": 12000,
    "estimated_tokens": 8734,
    "max_sessions": 8,
    "max_evidence": 24,
    "context_lines": 3,
    "max_excerpt_chars": 1600,
    "field_mask": "standard"
  },
  "realized": {
    "search_mode": "hybrid",
    "fallback_mode": "lexical",
    "semantic_joined": false,
    "candidate_count": 184,
    "selected_evidence_count": 18,
    "selected_session_count": 6
  },
  "health": {
    "healthy": true,
    "recommended_action": null,
    "index_state": "ready",
    "semantic_state": "model_absent",
    "active_rebuild": false,
    "source_readiness": []
  },
  "freshness": {
    "policy": "prefer-recent",
    "window_seconds": 2592000,
    "newest_evidence_at_ms": 1778219000000,
    "oldest_evidence_at_ms": 1775627000000,
    "stale_evidence_count": 2
  },
  "pack": {
    "title": "storage open recovery",
    "answer_outline": [],
    "source_summary": [],
    "handoff": []
  },
  "evidence": [],
  "omitted": {
    "count": 0,
    "items": []
  },
  "privacy": {
    "redaction_policy": "strict",
    "redaction_applied": false,
    "sensitive_output": false,
    "skill_content_included": false,
    "redaction_counts": {}
  },
  "warnings": []
}
```

## Evidence Item Schema

Each `evidence[]` item is an extractive span.

| Field | Type | Contract |
|-------|------|----------|
| `id` | string | Stable id: `ev_<base32(blake3(citation core))>`. |
| `rank` | integer | One-indexed final pack rank. |
| `excerpt` | string | UTF-8-safe extracted text after redaction. |
| `excerpt_truncated` | bool | True when `--max-excerpt-chars` shortened text. |
| `estimated_tokens` | integer | `ceil(char_count / 4)`, computed after redaction. |
| `citation` | object | Required citation fields below. |
| `selection` | object | Score summary fields below. |
| `roles` | array string | Session roles represented in the span. |
| `matched_terms` | array string | Normalized query terms found in the span. |
| `redactions` | array object | Redaction events when policy redacts content. |

## Pack Object Schema

`pack` is deterministic display scaffolding built from selected evidence. It is
not an LLM summary.

| Field | Type | Contract |
|-------|------|----------|
| `title` | string | Normalized query or explicit issue title. |
| `answer_outline` | array object | Evidence-cluster headings with citations. |
| `source_summary` | array object | Counts and readiness by source. |
| `handoff` | array object | Extractive next-step bullets with evidence ids. |

`answer_outline[]` fields:

| Field | Type | Contract |
|-------|------|----------|
| `rank` | integer | One-indexed outline rank. |
| `heading` | string | Deterministic heading from top matched terms and agent/workspace labels. |
| `evidence_ids` | array string | Evidence ids supporting this outline item. |

`source_summary[]` fields:

| Field | Type | Contract |
|-------|------|----------|
| `source_id` | string | Source id. |
| `origin_kind` | string | Local or remote origin kind. |
| `session_count` | integer | Selected sessions from this source. |
| `evidence_count` | integer | Selected evidence items from this source. |
| `newest_evidence_at_ms` | integer or null | Newest selected timestamp. |
| `healthy` | bool | Source health/readiness at pack time. |

`handoff[]` fields:

| Field | Type | Contract |
|-------|------|----------|
| `rank` | integer | One-indexed handoff rank. |
| `kind` | string | `fact`, `decision`, `blocker`, `next_step`, or `open_question`. |
| `text` | string | Extractive or template text derived from selected evidence. |
| `evidence_ids` | array string | Supporting evidence ids. |

Citation fields:

| Field | Type | Contract |
|-------|------|----------|
| `source_path` | string | Original session path from search output. |
| `source_id` | string | Normalized source id, default `local`. |
| `origin_kind` | string | `local`, `ssh`, or configured origin kind. |
| `origin_host` | string or null | Host label for remote sources. |
| `workspace` | string | Rewritten workspace path used by search. |
| `workspace_original` | string or null | Original workspace if rewritten. |
| `agent` | string | Agent slug. |
| `line_start` | integer or null | One-indexed first source line for the span. |
| `line_end` | integer or null | One-indexed last source line for the span. |
| `message_index` | integer or null | Zero-indexed message position when known. |
| `conversation_id` | integer or null | Internal DB conversation id when available. |
| `content_hash` | string | Hex hash of normalized content. |
| `span_hash` | string | Hex hash of the exact selected span before redaction. |
| `excerpt_sha256` | string | SHA-256 of emitted excerpt after redaction. |
| `created_at_ms` | integer or null | Source message/session timestamp. |
| `indexed_at_ms` | integer or null | Index timestamp when available. |
| `freshness_age_seconds` | integer or null | Age at pack generation time. |
| `match_type` | string | Existing search match type. |
| `verified` | bool | True when the cited path/span was read successfully. |

Selection fields:

| Field | Type | Contract |
|-------|------|----------|
| `score` | number | Final score after weights and penalties. |
| `relevance_score` | number | Normalized lexical/semantic relevance, `0.0..1.0`. |
| `coverage_score` | number | Query-term and phrase coverage, `0.0..1.0`. |
| `freshness_score` | number | Recency contribution, `0.0..1.0`. |
| `source_diversity_score` | number | Diversity boost at selection time. |
| `source_authority_score` | number | Deterministic source trust/readiness contribution. |
| `role_score` | number | Role/actionability contribution. |
| `citation_quality_score` | number | Provenance completeness contribution. |
| `duplicate_penalty` | number | Penalty applied for near-duplicate evidence. |
| `token_cost` | integer | Estimated token cost of item. |
| `selected_reason` | string | Primary reason this evidence was chosen. |

When `--explain-selection` is not set, `selection` keeps only `score`,
`token_cost`, and `selected_reason`.

## Omitted Item Schema

`omitted.items[]` explains why candidates did not fit the pack.

| Field | Type | Contract |
|-------|------|----------|
| `candidate_id` | string | Stable candidate id. |
| `source_path` | string | Candidate source path. |
| `line_start` | integer or null | Candidate start line. |
| `agent` | string | Candidate agent slug. |
| `reason` | string | One of the omitted reasons below. |
| `score` | number | Candidate score before omission. |
| `estimated_tokens` | integer | Candidate token estimate. |

Omitted reasons:

- `token_budget_exhausted`
- `max_sessions_reached`
- `max_evidence_reached`
- `duplicate_content`
- `same_session_lower_rank`
- `stale_under_strict_policy`
- `source_unavailable`
- `redacted_to_empty`
- `field_mask_excluded`

## Deterministic Selection Model

The pack planner starts from an over-fetched search result set:

```text
candidate_limit = max(max_evidence * 8, max_sessions * 16, 64)
candidate_limit = min(candidate_limit, 2048)
```

For every candidate span:

```text
score =
  0.35 * relevance_score +
  0.20 * coverage_score +
  0.15 * freshness_score +
  0.10 * source_diversity_score +
  0.10 * role_score +
  0.05 * source_authority_score +
  0.05 * citation_quality_score -
  duplicate_penalty
```

Tie-break order is stable:

1. Higher `score`.
2. Higher `relevance_score`.
3. Newer `created_at_ms`, with null timestamps last.
4. Lexicographic `source_id`.
5. Lexicographic `source_path`.
6. Lower `line_start`, with null lines last.
7. Lower `content_hash`.

Scoring inputs:

- `lexical_score`: BM25 or fallback lexical score from search.
- `semantic_score`: semantic similarity when available; otherwise null.
- `hybrid_rank`: RRF/hybrid rank when available.
- `query_term_hits`: count of normalized query terms present in the span.
- `query_phrase_hits`: count of normalized quoted phrases present.
- `created_at_ms`: timestamp used for recency.
- `source_id`, `origin_kind`, `origin_host`: source diversity keys.
- `source_health`, `source_readiness`, and explicit source filters: source
  authority keys.
- `agent`: agent diversity and role interpretation key.
- `role`: message role, when available.
- `source_path`, `line_start`, `line_end`: citation completeness.
- `content_hash` and `span_hash`: duplicate detection keys.

Score component definitions:

- `relevance_score`: normalize best available search score to `0.0..1.0` within
  the candidate set; hybrid rank maps by reciprocal rank when raw score is not
  comparable.
- `coverage_score`: `(term_hits + 2 * phrase_hits) / (query_terms + 2 *
  query_phrases)`, clamped to `1.0`.
- `freshness_score`: `1.0` for evidence inside the freshness window, then
  linear decay to `0.0` at four times the window. Null timestamps score `0.25`
  under `prefer-recent`, `1.0` under `allow-stale`, and `0.0` under `strict`.
- `source_diversity_score`: `1.0` when the source/session is not yet represented
  in the selected set, `0.5` when only the source is represented, `0.0` when the
  same session already has higher-ranked evidence.
- `source_authority_score`: `1.0` for local or explicitly requested healthy
  sources, `0.9` for healthy configured remote sources, `0.6` for stale but
  readable sources, and `0.4` for readable sources with incomplete readiness
  metadata.
- `role_score`: `1.0` for assistant conclusions and tool results, `0.85` for
  user requirements, `0.65` for tool-call arguments, `0.5` for unknown roles.
- `citation_quality_score`: `1.0` when path, source id, agent, and line span are
  all present; `0.75` when line span is missing; `0.5` when only path and agent
  are present.
- `duplicate_penalty`: `1.0` for exact duplicate span hash, `0.5` for same
  content hash, `0.25` for same source path and overlapping line range, else
  `0.0`.

## Token Budget Contract

Token estimates use the existing cass approximation: four UTF-8 characters per
token, rounded up. The planner reserves budget as follows:

| Section | Budget |
|---------|--------|
| Metadata, limits, health, freshness, privacy | 15% |
| Pack outline and handoff bullets | 15% |
| Evidence excerpts and citations | 60% |
| Omitted reasons and warnings | 10% |

If evidence exceeds its section budget, the planner first shortens excerpts,
then drops lowest-ranked evidence while recording `token_budget_exhausted`
omissions. It never drops citation fields for selected evidence.

`estimated_tokens` must be computed after redaction and truncation. The emitted
pack may exceed `max_tokens` by at most 5% because JSON/TOON syntax overhead is
format-dependent; if it exceeds that tolerance, the planner must drop another
evidence item or return `err.kind="pack-budget-too-small"`.

## Freshness and Health Proof

Every pack includes health fields derived from the same truth surfaces used by
`cass health --json` and `cass status --json`.

Required health fields:

- `healthy`
- `recommended_action`
- `index_state`
- `semantic_state`
- `fallback_mode`
- `active_rebuild`
- `source_readiness[]`

Each `source_readiness[]` item includes:

- `source_id`
- `origin_kind`
- `healthy`
- `last_sync_at_ms`
- `last_indexed_at_ms`
- `recommended_action`

The pack command must not hide degraded semantic readiness. If semantic assets
are absent but lexical search is ready, the pack succeeds with
`realized.fallback_mode="lexical"` and a warning only when `--mode semantic` was
explicitly requested.

## Privacy Contract

Default redaction policy is `strict`.

Strict redaction removes or masks:

- API keys and tokens.
- Bearer tokens, cookies, and authorization headers.
- JWT-like strings.
- Passwords and password assignments.
- Private keys and SSH keys.
- `.env` assignments that include secret-like names.
- Webhook secrets.
- Database URLs with credentials.
- High-entropy strings longer than 32 characters.
- Proprietary skill content unless `--include-skill-content` is set.

Balanced redaction keeps non-secret identifiers and short hashes but still
redacts credentials, keys, passwords, cookies, private keys, and skill content.

Redaction invariants:

- Redaction never changes citation path or line fields.
- Redaction must preserve excerpt UTF-8 validity.
- Redaction must emit `redactions[]` with `kind`, `start_char`, `end_char`, and
  `replacement`.
- Redaction must set `privacy.redaction_applied=true` when any excerpt changed.
- If redaction removes all meaningful text, omit the candidate with
  `reason="redacted_to_empty"`.
- Pack output never reads `.env` directly and never includes `.env` contents
  unless they already appear in indexed session evidence and pass the selected
  redaction policy.

## Error Contract

Pack errors use the existing `CliError` envelope and kebab-case `err.kind`.
Consumers must branch on `err.kind`, not just numeric code.

| Kind | Code | Retryable | When |
|------|------|-----------|------|
| `pack-empty-query` | 2 | false | Query is empty after normalization. |
| `pack-invalid-limit` | 2 | false | A pack-specific limit is outside allowed range. |
| `pack-invalid-field` | 2 | false | Field mask has no valid fields. |
| `pack-invalid-redaction-policy` | 2 | false | Unknown redaction policy. |
| `pack-unsupported-format` | 2 | false | `--robot-format sessions` or unsupported display format. |
| `missing-db` | 3 | true | Database is absent. |
| `missing-index` | 3 | true | Lexical index is absent and cannot be refreshed. |
| `not-found` | 13 | false | No evidence and `--require-evidence` is set. |
| `lock-busy` | 7 | true | Existing lock/busy condition. |
| `partial-result` | 8 | true | Timeout produced partial evidence. |
| `timeout` | 10 | true | Timeout before any usable pack could be emitted. |
| `config` | 10 | false | Invalid data-dir, db path, or source config. |
| `semantic-unavailable` | 15 | true | Explicit semantic mode cannot run. |
| `io` | 14 | true | Source path or citation read failed unexpectedly. |

Empty search results are success by default:

```json
{
  "schema_version": "cass.pack.v1",
  "evidence": [],
  "omitted": {"count": 0, "items": []},
  "warnings": ["no_evidence_found"]
}
```

## Format Contracts

JSON is pretty-printed by default. Compact JSON is a single-line object with the
same fields. TOON encodes the same payload with the existing `toon` crate path.

JSONL emits one object per line:

1. `{"_meta": ...}`
2. `{"pack": ...}`
3. One line per `evidence` item.
4. One line with `{"omitted": ...}`
5. One line with `{"privacy": ...}`

Markdown output must include citations inline:

```markdown
# storage open recovery

## Handoff

- Evidence-backed bullet. [ev_abc123]

## Evidence

[ev_abc123] codex local /path/session.jsonl:42-47
```

Markdown is not a replacement for robot JSON; implementation must add JSON
goldens first.

## Implementation Boundaries

- Put command parsing in `src/lib.rs` with existing CLI patterns.
- Put selection logic in a small module under `src/search/` or `src/pack/` only
  if it cannot stay readable in existing search code.
- Reuse `SearchClient` and existing `SearchFilters`; do not duplicate search
  index logic.
- Use `frankensqlite` for any new SQLite access.
- Read session spans through existing `view`/`expand` style helpers where
  possible so source mapping behavior stays consistent.
- Do not mutate source logs, indexes, quarantine directories, or health state.

## Required Test Obligations

Unit tests:

- deterministic score ordering and tie-breaks.
- token-budget truncation and evidence dropping.
- duplicate suppression by `span_hash`, `content_hash`, and overlapping line
  range.
- field-mask projection.
- strict, balanced, and off redaction policies.
- `--robot-format sessions` rejection.
- empty result success and `--require-evidence` error.

Golden tests:

- `cass pack "..." --json`
- `cass pack "..." --robot-format compact`
- `cass pack "..." --robot-format jsonl`
- `cass pack "..." --robot-format toon`
- `cass pack "..." --display markdown`
- `cass introspect --json` schema update for `pack`.
- `cass robot-docs schemas` and `cass robot-docs guide` updates.

Conformance tests:

- same fixture produces byte-stable JSON after sorting non-deterministic maps.
- same fixture with missing semantic model reports lexical fallback.
- same fixture with a stale source reports source readiness truthfully.
- privacy fixtures prove secrets are absent from emitted excerpts.

No-mock integration fixture:

- create real temporary session fixtures.
- index them through the normal indexing path.
- run `cass pack --json`.
- assert citations resolve to readable source lines.
- do not run browser/Playwright tests locally.

Verification commands for implementation beads:

```bash
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo fmt --check
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo check --all-targets
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo clippy --all-targets -- -D warnings
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo test pack
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo test --test golden_robot_json --test golden_robot_docs
```

If goldens change intentionally, regenerate with:

```bash
UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo test --test golden_robot_json --test golden_robot_docs
```

Review `git diff tests/golden/` before committing regenerated files.
