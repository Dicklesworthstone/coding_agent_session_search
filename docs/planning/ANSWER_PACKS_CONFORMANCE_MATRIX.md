# Answer-Pack Conformance Matrix

**Spec:** `docs/planning/ANSWER_PACKS_CONTRACT.md`
**Bead:** `coding_agent_session_search-uuwye.6`
**Status:** Draft coverage map for implementation and golden gates
**Date:** 2026-05-08

This matrix turns the answer-pack contract into testable requirements. It is not
evidence of conformance by itself. A row is conformant only when the named test
exists, passes, and asserts the contract behavior directly instead of relying on
a proxy signal.

`Current Tested` counts rows with full contract-surface coverage. Planner-only
unit tests are recorded in the row status and evidence ledger, but they do not
count as pack conformance until the command, renderer, schema, and golden
surfaces exist.

## Coverage Accounting

| Spec Area | MUST Clauses | SHOULD Clauses | Current Tested | Passing | Divergent | Score |
|-----------|-------------:|---------------:|---------------:|--------:|----------:|-------|
| Command surface and input validation | 19 | 0 | 0 | 0 | 0 | 0/19 |
| Field masks and schema stability | 18 | 0 | 0 | 0 | 0 | 0/18 |
| Evidence, citations, pack objects | 34 | 0 | 0 | 0 | 0 | 0/34 |
| Selection and omission behavior | 30 | 0 | 0 | 0 | 0 | 0/30 |
| Token budget and partial output | 7 | 0 | 0 | 0 | 0 | 0/7 |
| Health, freshness, and source readiness | 15 | 0 | 0 | 0 | 0 | 0/15 |
| Privacy and redaction | 21 | 0 | 0 | 0 | 0 | 0/21 |
| Error envelopes and output formats | 25 | 0 | 0 | 0 | 0 | 0/25 |
| Implementation boundaries | 10 | 0 | 0 | 0 | 0 | 0/10 |
| Required verification commands | 6 | 0 | 0 | 0 | 0 | 0/6 |

## Idea-Wizard Artifact Map

This section is the durable phase-4/phase-6 overlap check for the answer-pack
idea-wizard run. The epic notes record that phases 2 and 3 generated the best
15 ideas; this table maps each idea to the current bead graph so future agents
can refine the plan without reconstructing the original prompt exchange.

| Rank | Idea | Current Bead Coverage | Phase-6 Verdict |
|------|------|-----------------------|-----------------|
| 1 | Deterministic answer packs for swarm handoffs | `coding_agent_session_search-uuwye`, `.1`, `.2`, `.3`, `.5` | Covered. Keep planner, renderers, and CLI separate so agents can ship and test them in dependency order. |
| 2 | Freshness and SLO proof on handoffs | `.4`, `.8` | Covered. Health/readiness metadata and latency budgets stay separate because correctness and performance need different fixtures. |
| 3 | Privacy-safe evidence bundles | `.9`, `.7`, conformance rows AP-PRIV-* | Covered, but must stay blocking for CLI exposure so early robot users do not receive unredacted handoff artifacts. |
| 4 | Contract and golden gates | `.6`, this matrix, `ANSWER_PACKS_CONTRACT.md` | Covered. Planner-only evidence remains explicitly non-conformant until command, renderer, schema, and no-mock layers exist. |
| 5 | No-mock e2e handoff fixtures | `.7`, `.6` | Covered. The fixture must index real temporary sessions and prove source logs are not mutated. |
| 6 | Source-readiness metadata | `.4`, AP-HEALTH-* rows | Covered. Reuse `health` and `status` truth surfaces instead of creating a pack-only readiness model. |
| 7 | TOON and Markdown output | `.3`, `.5`, AP-FMT-* rows | Covered. JSON remains canonical; Markdown and TOON are format projections with equality/golden gates. |
| 8 | `--sessions-from` stdin/file input | `.5`, AP-CMD-007 | Covered. Needs CLI tests for stdin, file input, and invalid paths without inventing a new session resolver. |
| 9 | Token-budget diagnostics | `.2`, `.6`, `.8`, AP-BUDGET-* rows | Covered. Budget behavior must be unit-tested before performance SLOs can be trusted. |
| 10 | Schema introspection and robot-docs discovery | `.5`, `.10`, AP-CMD-002, AP-GATE-* | Covered. `cass introspect --json` and `robot-docs` must expose pack without requiring source reads. |
| 11 | Omitted-item reasons | `.2`, `.3`, AP-OMIT-* rows | Covered. Omission reasons are part of the robot contract, not just internal planner diagnostics. |
| 12 | Deterministic ordering and tie-breaks | `.2`, `.6`, AP-SEL-* rows | Covered. Keep input-order stability, null timestamp ordering, and later tie-break cases explicit. |
| 13 | Field masks for minimal/standard/full outputs | `.5`, `.6`, AP-MASK-* rows | Covered. Mask behavior belongs in CLI/golden tests because consumers depend on exact payload shape. |
| 14 | Boundary/non-goal guarantees | `.6`, `.7`, AP-BOUND-* rows | Covered. Pack must not call external LLMs, auto-download models, mutate source logs, run doctor fixes, or clean derived assets. |
| 15 | Operator and agent docs | `.10`, AP-GATE-* rows | Covered. Docs should land after contracts and tests so examples match the shipped command. |

No additional beads are needed for these ideas at this time. If a future phase-6
pass finds a new requirement, it should first extend this matrix or the
contract, then update `.beads` with `br` only after active metadata reservations
are clear.

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
| AP-CMD-010 | MUST | Input Flags | `--require-evidence` turns an empty pack into `err.kind="not-found"` with code 13. | CLI fixture with no hits. | Planned |
| AP-CMD-011 | MUST | Input Flags | `--explain-selection` exposes score components and omission diagnostics. | Planner unit test plus full field-mask golden. | Planned |
| AP-CMD-012 | MUST | Input Flags | `--refresh` runs only the existing safe incremental refresh path before packing. | CLI integration fixture that snapshots index/quarantine state and verifies no hidden repair path runs. | Planned |
| AP-CMD-013 | MUST | Input Flags | `--timeout <ms>` marks usable partial packs as partial and returns timeout/partial errors truthfully when no usable pack can be emitted. | Timeout fixture covering partial success and no-pack failure. | Planned |
| AP-MASK-001 | MUST | Field Masks | `standard` includes exactly the documented top-level fields. | JSON schema/golden assertion. | Planned |
| AP-MASK-002 | MUST | Field Masks | `minimal` includes only the documented dotted fields. | Minimal mask golden. | Planned |
| AP-MASK-003 | MUST | Field Masks | `full` includes all defined response fields and `selection_debug` only when requested. | Full mask golden pair. | Planned |
| AP-MASK-004 | MUST | Field Masks | Unknown explicit mask fields are ignored with `_meta.warnings[]` and exit 0. | CLI warning test. | Planned |
| AP-MASK-005 | MUST | Field Masks | Explicit field masks with no valid fields fail with `err.kind="pack-invalid-field"`. | CLI error-envelope test with all-invalid field list. | Planned |
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
| AP-SEL-003 | MUST | Deterministic Selection | Tie-break order is stable across equal score, relevance, timestamp, source, path, line, and content hash. | `stable_tie_breaks_do_not_depend_on_input_order` covers input-order stability through source path; table cases for every later tie-break remain. | Unit Partial |
| AP-SEL-004 | MUST | Deterministic Selection | Null timestamps sort last in tie-breaks and score according to freshness policy. | `strict_freshness_omits_stale_or_unknown_timestamps` covers strict stale/unknown omission; prefer-recent null tie-break still needs a direct case. | Unit Partial |
| AP-SEL-005 | MUST | Deterministic Selection | Diversity scoring changes as selected sources and sessions accumulate. | `source_diversity_changes_second_pick` covers the greedy second-pick source diversity decision. | Planner Unit Passing |
| AP-SEL-006 | MUST | Deterministic Selection | Duplicate penalties cover span hash, content hash, and overlapping source ranges. | `duplicate_content_is_omitted_after_first_selection` covers content-hash duplicate omission; span-hash and range-overlap cases remain. | Unit Partial |
| AP-SEL-007 | MUST | Selection Fields | Without `--explain-selection`, `selection` exposes only score, token cost, and selected reason. | JSON golden pair with and without explain-selection. | Planned |
| AP-OMIT-001 | MUST | Omitted Item Schema | Omitted rows include stable candidate id, source path, line, agent, reason, score, and estimated tokens. | Omission schema unit test. | Planned |
| AP-OMIT-002 | MUST | Omitted Reasons | Reasons are exactly the documented snake_case values. | Enum serialization/schema test. | Planned |
| AP-OMIT-003 | MUST | Omitted Reasons | Hard-omitted candidates are emitted once and removed from future consideration. | `duplicate_content_is_omitted_after_first_selection` covers the duplicate-content hard omission; stale/redacted variants remain. | Unit Partial |
| AP-OMIT-004 | MUST | Omitted Reasons | Budget-omitted candidates are emitted once and not later emitted as max evidence. | `exact_token_budget_boundary_selects_until_budget_exhausted` covers token-budget omission at the exact boundary. | Unit Partial |
| AP-BUDGET-001 | MUST | Token Budget | Token estimates use ceil UTF-8 char count divided by four after redaction and truncation. | Token estimator unit test. | Planned |
| AP-BUDGET-002 | MUST | Token Budget | Budget reserves 15% metadata, 15% outline, 60% evidence, 10% omitted/warnings. | Planner budget unit test. | Planned |
| AP-BUDGET-003 | MUST | Token Budget | Planner shortens excerpts before dropping evidence. | Truncation/drop ordering test. | Planned |
| AP-BUDGET-004 | MUST | Token Budget | Selected evidence never loses citation fields to fit budget. | Small budget JSON golden. | Planned |
| AP-BUDGET-005 | MUST | Token Budget | Output may exceed max tokens by no more than 5%; otherwise drop another item or return `pack-budget-too-small`. | `exact_token_budget_boundary_selects_until_budget_exhausted` covers dropping the next item at an exact boundary; overshoot tolerance and too-small error remain. | Unit Partial |
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
| AP-PRIV-010 | MUST | Privacy Contract | Skill payload excerpts are included only when `--include-skill-content` is explicitly set. | Privacy fixture pair for default exclusion and explicit inclusion. | Planned |
| AP-PRIV-011 | MUST | Privacy Contract | `--redaction off` is accepted only for local operator workflows and is visibly marked sensitive. | CLI/privacy fixture for allowed local use and rejected non-local use. | Planned |
| AP-ERR-001 | MUST | Error Contract | Errors use existing `CliError` envelope and kebab-case `err.kind`. | Error-envelope tests for every pack kind. | Planned |
| AP-ERR-002 | MUST | Error Contract | Consumers can branch on `err.kind`; numeric code alone is not relied on. | Introspect/docs schema test. | Planned |
| AP-ERR-003 | MUST | Error Contract | Empty search results succeed by default with `no_evidence_found`. | Empty fixture golden. | Planned |
| AP-ERR-004 | MUST | Error Contract | `pack-invalid-field`, `pack-budget-too-small`, `partial-result`, and `timeout` use documented codes, retryability, and hints. | Table-driven error-envelope regression. | Planned |
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
| AP-BOUND-006 | MUST | Non-Goals | Pack does not call external LLMs or rewrite evidence into model-generated summaries. | Code-path audit plus fixture proving pack output is derived from selected evidence only. | Planned |
| AP-BOUND-007 | MUST | Non-Goals | Pack never auto-downloads semantic models; missing models must use truthful lexical fallback or semantic errors. | Missing-model fixture that checks no model artifact is created and no acquisition path runs. | Planned |
| AP-BOUND-008 | MUST | Non-Goals | Pack does not run hidden `doctor --fix`, manual index repair, or quarantine garbage collection. | State-snapshot integration test around pack execution. | Planned |
| AP-BOUND-009 | MUST | Non-Goals | Pack does not delete or clean up derived assets while preparing output. | State-snapshot integration test covering index, quarantine, and publish-backup directories. | Planned |
| AP-GATE-001 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo fmt --check` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-002 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo check --all-targets` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-003 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo clippy --all-targets -- -D warnings` is part of closeout. | Implementation closeout evidence. | Planned |
| AP-GATE-004 | MUST | Verification Commands | `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-answer-pack-target cargo test pack` is part of closeout. | Implementation closeout evidence. | Planned |
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

## Planner Unit Evidence

The current planner-only proof set lives in `src/search/pack_planner.rs`:

| Test | Rows Informed | Limit |
|------|---------------|-------|
| `empty_corpus_returns_empty_plan` | AP-CMD-010, AP-ERR-003 | Planner only; command error/default behavior still needs robot fixtures. |
| `duplicate_content_is_omitted_after_first_selection` | AP-SEL-006, AP-OMIT-003 | Covers content-hash duplicates only. |
| `exact_token_budget_boundary_selects_until_budget_exhausted` | AP-BUDGET-005, AP-OMIT-004 | Covers exact boundary drop only. |
| `source_diversity_changes_second_pick` | AP-SEL-005 | Covers planner selection, not rendered diagnostics. |
| `strict_freshness_omits_stale_or_unknown_timestamps` | AP-SEL-004 | Covers strict policy omission, not prefer-recent tie order. |
| `lexical_score_drives_relevance_when_semantic_is_absent` | AP-SCHEMA-003, AP-HEALTH-004 | Planner score proof only; realized mode/fallback metadata still needs command fixtures. |
| `stable_tie_breaks_do_not_depend_on_input_order` | AP-SEL-003 | Covers source-path stability only. |

## Known Draft Gaps

- Planner unit tests now exist for several selection rows, but they do not by
  themselves satisfy this matrix. Full conformance also requires the pack command,
  robot schemas, golden outputs, privacy fixtures, and no-mock citation
  resolution.
- Planner work is still in progress under `coding_agent_session_search-uuwye.2`.
- The pack conformance gate must not be marked passing from planner unit tests
  alone; robot output and no-mock citation resolution are separate obligations.
- Planner unit rows above were verified against `c9eafb8b` with
  `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_olive_pack_planner_review cargo test pack_planner --lib -- --nocapture`
  passing 7/7, but they remain below full contract-surface conformance.
