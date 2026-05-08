# Answer-Pack Conformance Matrix

**Spec:** `docs/planning/ANSWER_PACKS_CONTRACT.md`
**Bead:** `coding_agent_session_search-uuwye.6`
**Status:** Draft coverage map for implementation and golden gates
**Date:** 2026-05-08

This matrix turns the answer-pack contract into testable requirements. It is not
evidence of conformance by itself. A row is conformant only when the named test
exists, passes, and asserts the contract behavior directly instead of relying on
a proxy signal.

## Coverage Accounting

| Spec Area | MUST Clauses | SHOULD Clauses | Current Tested | Passing | Divergent | Score |
|-----------|-------------:|---------------:|---------------:|--------:|----------:|-------|
| Command surface and input validation | 17 | 0 | 0 | 0 | 0 | 0/17 |
| Field masks and schema stability | 17 | 0 | 0 | 0 | 0 | 0/17 |
| Evidence, citations, pack objects | 34 | 0 | 0 | 0 | 0 | 0/34 |
| Selection and omission behavior | 29 | 0 | 0 | 0 | 0 | 0/29 |
| Token budget and partial output | 7 | 0 | 0 | 0 | 0 | 0/7 |
| Health, freshness, and source readiness | 15 | 0 | 0 | 0 | 0 | 0/15 |
| Privacy and redaction | 19 | 0 | 0 | 0 | 0 | 0/19 |
| Error envelopes and output formats | 24 | 0 | 0 | 0 | 0 | 0/24 |
| Implementation boundaries | 6 | 0 | 0 | 0 | 0 | 0/6 |
| Required verification commands | 6 | 0 | 0 | 0 | 0 | 0/6 |

## Requirement Matrix

| ID | Level | Contract Source | Requirement | Planned Coverage | Status |
|----|-------|-----------------|-------------|------------------|--------|
| AP-CMD-001 | MUST | Command Surface | `cass pack <query>` exists and runs only through robot or explicit display output. | CLI robot integration test. | Planned |
| AP-CMD-002 | MUST | Command Surface | `--json`, compact JSON, JSONL, and TOON robot formats are accepted. | Golden robot JSON docs suite plus pack goldens. | Planned |
| AP-CMD-003 | MUST | Command Surface | Markdown requires `--display markdown`. | CLI argument test and Markdown golden. | Planned |
| AP-CMD-004 | MUST | Command Surface | `--robot-format sessions` returns `err.kind="pack-unsupported-format"`. | CLI error-envelope test. | Planned |
| AP-CMD-005 | MUST | Input Flags | Empty query returns `pack-empty-query`. | CLI error-envelope test. | Planned |
| AP-CMD-006 | MUST | Input Flags | Search filters are reused for agent, workspace, source, time, mode, approximate, refresh, timeout, request id, data dir, and db. | Table-driven CLI parsing and one fixture for filter propagation. | Planned |
| AP-CMD-007 | MUST | Input Flags | `--sessions-from <path|->` reads one candidate session path per line, including stdin. | No-mock integration fixture with file and stdin variants. | Planned |
| AP-CMD-008 | MUST | Input Flags | Pack limits enforce documented minimums and maximums. | Table-driven invalid-limit tests. | Planned |
| AP-CMD-009 | MUST | Input Flags | `--redaction off` marks `privacy.redaction_policy="off"` and `sensitive_output=true`. | Privacy unit test and JSON golden. | Planned |
| AP-CMD-010 | MUST | Input Flags | `--require-evidence` turns an empty pack into `pack-no-evidence`. | CLI fixture with no hits. | Planned |
| AP-CMD-011 | MUST | Input Flags | `--explain-selection` exposes score components and omission diagnostics. | Planner unit test plus full field-mask golden. | Planned |
| AP-MASK-001 | MUST | Field Masks | `standard` includes exactly the documented top-level fields. | JSON schema/golden assertion. | Planned |
| AP-MASK-002 | MUST | Field Masks | `minimal` includes only the documented dotted fields. | Minimal mask golden. | Planned |
| AP-MASK-003 | MUST | Field Masks | `full` includes all defined response fields and `selection_debug` only when requested. | Full mask golden pair. | Planned |
| AP-MASK-004 | MUST | Field Masks | Unknown explicit mask fields are ignored with `_meta.warnings[]` and exit 0. | CLI warning test. | Planned |
| AP-SCHEMA-001 | MUST | JSON Response Schema | All object keys are stable and snake_case. | Golden plus schema walk rejecting non-snake keys. | Planned |
| AP-SCHEMA-002 | MUST | JSON Response Schema | `_meta.partial`, format, request id, generated time, elapsed time, and warnings are present. | Scrubbed JSON golden. | Planned |
| AP-SCHEMA-003 | MUST | JSON Response Schema | `realized` truthfully reports search mode, fallback, semantic join, candidates, selected evidence, and selected sessions. | Fixture covering lexical fallback and semantic unavailable. | Planned |
| AP-EV-001 | MUST | Evidence Item Schema | Evidence ids use `ev_<base32(blake3(citation core))>` and are stable. | Deterministic id unit test. | Planned |
| AP-EV-002 | MUST | Evidence Item Schema | Evidence rank is one-indexed final pack rank. | Planner ordering unit test. | Planned |
| AP-EV-003 | MUST | Evidence Item Schema | Excerpts are UTF-8-safe, redacted before token estimation, and mark truncation. | Redaction and truncation unit tests. | Planned |
| AP-EV-004 | MUST | Evidence Item Schema | Every selected evidence item includes citation, selection, roles, matched terms, and redactions fields. | JSON schema/golden assertion. | Planned |
| AP-CIT-001 | MUST | Citation Fields | Citation carries path, source id, origin kind, workspace, agent, line/message positions, ids, hashes, timestamps, match type, and verification status. | Citation schema test with complete fixture. | Planned |
| AP-CIT-002 | MUST | Citation Fields | Citation path and line fields resolve to readable source lines in the no-mock fixture. | No-mock integration fixture. | Planned |
| AP-CIT-003 | MUST | Citation Fields | `match_type` uses the existing search robot spelling, not Rust debug variant names. | Aggregation/pack regression for `implicit_wildcard`. | Planned |
| AP-PACK-001 | MUST | Pack Object Schema | Pack title, answer outline, source summary, and handoff are deterministic display scaffolding, not LLM summaries. | Unit fixture comparing exact output from fixed evidence. | Planned |
| AP-PACK-002 | MUST | Pack Object Schema | Outline headings are deterministic from matched terms and labels. | Planner/render unit test. | Planned |
| AP-PACK-003 | MUST | Pack Object Schema | Source summaries include source id, origin kind, session count, evidence count, newest timestamp, and health. | Source readiness fixture. | Planned |
| AP-PACK-004 | MUST | Pack Object Schema | Handoff items use allowed kinds and cite supporting evidence ids. | Renderer schema test. | Planned |
| AP-SEL-001 | MUST | Deterministic Selection | Candidate over-fetch uses `max(max_evidence * 8, max_sessions * 16, 64)` capped at 2048. | Planner config unit test. | Planned |
| AP-SEL-002 | MUST | Deterministic Selection | Score uses the documented component weights and duplicate penalty. | Score-component unit test. | Planned |
| AP-SEL-003 | MUST | Deterministic Selection | Tie-break order is stable across equal score, relevance, timestamp, source, path, line, and content hash. | Table-driven tie-break unit tests. | Planned |
| AP-SEL-004 | MUST | Deterministic Selection | Null timestamps sort last in tie-breaks and score according to freshness policy. | Timestamp policy unit test. | Planned |
| AP-SEL-005 | MUST | Deterministic Selection | Diversity scoring changes as selected sources and sessions accumulate. | Greedy selection unit test. | Planned |
| AP-SEL-006 | MUST | Deterministic Selection | Duplicate penalties cover span hash, content hash, and overlapping source ranges. | Duplicate-heavy planner unit tests. | Planned |
| AP-OMIT-001 | MUST | Omitted Item Schema | Omitted rows include stable candidate id, source path, line, agent, reason, score, and estimated tokens. | Omission schema unit test. | Planned |
| AP-OMIT-002 | MUST | Omitted Reasons | Reasons are exactly the documented snake_case values. | Enum serialization/schema test. | Planned |
| AP-OMIT-003 | MUST | Omitted Reasons | Hard-omitted candidates are emitted once and removed from future consideration. | Planner regression for stale/duplicate hard omissions. | Planned |
| AP-OMIT-004 | MUST | Omitted Reasons | Budget-omitted candidates are emitted once and not later emitted as max evidence. | Planner regression for exact budget boundary. | Planned |
| AP-BUDGET-001 | MUST | Token Budget | Token estimates use ceil UTF-8 char count divided by four after redaction and truncation. | Token estimator unit test. | Planned |
| AP-BUDGET-002 | MUST | Token Budget | Budget reserves 15% metadata, 15% outline, 60% evidence, 10% omitted/warnings. | Planner budget unit test. | Planned |
| AP-BUDGET-003 | MUST | Token Budget | Planner shortens excerpts before dropping evidence. | Truncation/drop ordering test. | Planned |
| AP-BUDGET-004 | MUST | Token Budget | Selected evidence never loses citation fields to fit budget. | Small budget JSON golden. | Planned |
| AP-BUDGET-005 | MUST | Token Budget | Output may exceed max tokens by no more than 5%; otherwise drop another item or return `pack-budget-too-small`. | Boundary fixture. | Planned |
| AP-HEALTH-001 | MUST | Freshness and Health Proof | Health fields come from the same truth surfaces as `cass health --json` and `cass status --json`. | Fixture with stubbed existing health/status surfaces, not ad hoc values. | Planned |
| AP-HEALTH-002 | MUST | Freshness and Health Proof | Required health fields include healthy, recommended action, index state, semantic state, fallback mode, active rebuild, and source readiness. | Health schema golden. | Planned |
| AP-HEALTH-003 | MUST | Freshness and Health Proof | Source readiness includes source id, origin kind, healthy, last sync, last indexed, and recommended action. | Source readiness fixture. | Planned |
| AP-HEALTH-004 | MUST | Freshness and Health Proof | Missing semantic assets with ready lexical search succeed as lexical fallback. | Semantic-absent conformance fixture. | Planned |
| AP-HEALTH-005 | MUST | Freshness and Health Proof | Explicit `--mode semantic` warns or errors truthfully when semantic is unavailable. | CLI semantic unavailable test. | Planned |
| AP-PRIV-001 | MUST | Privacy Contract | Default redaction policy is strict. | JSON golden. | Planned |
| AP-PRIV-002 | MUST | Privacy Contract | Strict redaction masks all listed secret classes and skill content unless explicitly included. | Privacy fixture matrix. | Planned |
| AP-PRIV-003 | MUST | Privacy Contract | Balanced redaction preserves non-secret identifiers and short hashes while still redacting credential classes. | Privacy fixture matrix. | Planned |
| AP-PRIV-004 | MUST | Privacy Contract | Redaction never changes citation path or line fields. | Redaction/citation invariant test. | Planned |
| AP-PRIV-005 | MUST | Privacy Contract | Redaction preserves UTF-8 validity. | Unicode secret fixture. | Planned |
| AP-PRIV-006 | MUST | Privacy Contract | Redaction events include kind, start char, end char, and replacement. | Redaction event schema test. | Planned |
| AP-PRIV-007 | MUST | Privacy Contract | `privacy.redaction_applied=true` when any excerpt changes. | JSON fixture. | Planned |
| AP-PRIV-008 | MUST | Privacy Contract | Fully redacted candidates are omitted with `redacted_to_empty`. | Redaction unit test. | Planned |
| AP-PRIV-009 | MUST | Privacy Contract | Pack never reads `.env` directly and includes `.env` content only when indexed evidence passes policy. | No-mock fixture with `.env` file present and indexed secret text. | Planned |
| AP-ERR-001 | MUST | Error Contract | Errors use existing `CliError` envelope and kebab-case `err.kind`. | Error-envelope tests for every pack kind. | Planned |
| AP-ERR-002 | MUST | Error Contract | Consumers can branch on `err.kind`; numeric code alone is not relied on. | Introspect/docs schema test. | Planned |
| AP-ERR-003 | MUST | Error Contract | Empty search results succeed by default with `no_evidence_found`. | Empty fixture golden. | Planned |
| AP-FMT-001 | MUST | Format Contracts | Pretty JSON and compact JSON contain the same fields. | Structural equality test after parsing. | Planned |
| AP-FMT-002 | MUST | Format Contracts | JSONL emits meta, pack, evidence items, omitted, and privacy in documented order. | JSONL golden. | Planned |
| AP-FMT-003 | MUST | Format Contracts | TOON encodes the same payload through the existing `toon` crate path. | JSON-vs-TOON decoded equality test if decoder is available; otherwise golden. | Planned |
| AP-FMT-004 | MUST | Format Contracts | Markdown includes inline evidence ids and an evidence section with path and line range. | Markdown golden. | Planned |
| AP-FMT-005 | MUST | Format Contracts | JSON goldens land before Markdown is treated as conformant. | Gate checklist in test docs. | Planned |
| AP-BOUND-001 | MUST | Implementation Boundaries | CLI parsing follows existing `src/lib.rs` patterns. | Code review checklist and CLI tests. | Planned |
| AP-BOUND-002 | MUST | Implementation Boundaries | Selection logic reuses `SearchClient` and existing filters instead of duplicating index logic. | Unit test via injected search outputs plus code review. | Planned |
| AP-BOUND-003 | MUST | Implementation Boundaries | New SQLite access uses frankensqlite only. | `rg "rusqlite"` delta check plus review. | Planned |
| AP-BOUND-004 | MUST | Implementation Boundaries | Session spans are read through existing view/expand-style helpers where possible. | Citation resolution integration test. | Planned |
| AP-BOUND-005 | MUST | Implementation Boundaries | Pack does not mutate source logs, indexes, quarantine directories, or health state. | No-mock fixture snapshots source/index metadata before and after. | Planned |
| AP-GATE-001 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/tmp/cass-answer-pack-target cargo fmt --check` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-002 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/tmp/cass-answer-pack-target cargo check --all-targets` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-003 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/tmp/cass-answer-pack-target cargo clippy --all-targets -- -D warnings` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-004 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/tmp/cass-answer-pack-target cargo test pack` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-005 | MUST | Verification Commands | Golden robot JSON/docs tests run through `rch exec` and reviewed diffs. | Implementation closeout evidence. | Planned |
| AP-GATE-006 | MUST | Verification Commands | Intentional golden updates use `UPDATE_GOLDENS=1 rch exec -- ...` and reviewed `tests/golden/` diffs. | Implementation closeout evidence. | Planned |

## Harness Shape

Use three layers:

1. Planner unit tests in the planner module for deterministic scoring,
   duplicate/omission handling, source diversity, freshness, and token budgets.
2. Robot golden tests alongside `tests/golden/robot/` and
   `tests/golden/robot_docs/` for schema, formatting, docs, and introspect.
3. A no-mock CLI integration fixture under `tests/` that creates real temporary
   session data, indexes it through the normal path, runs `cass pack --json`, and
   verifies citations resolve without mutating source logs.

## Known Draft Gaps

- Planner unit tests now exist for several selection rows, but they do not by
  themselves satisfy this matrix. Full conformance also requires the pack command,
  robot schemas, golden outputs, privacy fixtures, and no-mock citation
  resolution.
- Planner work is still in progress under `coding_agent_session_search-uuwye.2`.
- The pack conformance gate must not be marked passing from planner unit tests
  alone; robot output and no-mock citation resolution are separate obligations.
