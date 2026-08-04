# Resilience Test Matrix & Closure Checklist

Bead: `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.12.1`.

This is the self-contained map from every resilience epic and its feature
families to the **exact proof** expected before a bead in that family may be
closed. It exists so a "just add tests" instruction can never let an
important failure mode slip through, and so reviewers and closure audits
share one definition of "done".

It pairs with:
- **`.12.2`** — the bounded E2E runner with structured logs.
- **`.12.3`** — the proof logging schema, artifact manifest, and retention.
- **`.12.6`** — the CI/local proof recipe and log-completeness gate.
- **`.11.*`** — the real-binary proof gates and regression corpus.

## How to read this

Each epic lists its feature families. Each family names the proof level that
is **mandatory for closure** and, where useful, **optional** live
diagnostics that strengthen but do not gate it.

Proof levels (weakest → strongest):

| Level | Meaning |
|-------|---------|
| `unit` | In-crate `#[cfg(test)]` over pure schema / classifier / planner / projection logic, incl. negative and boundary cases. Runs without I/O. |
| `integration` | In-crate or `tests/` exercising real types across modules (e.g. real `QuarantineState::load`, real storage), no live network/models. |
| `golden` | A pinned artifact (JSON/JSONL/snapshot) that a change must deliberately update; detects silent wire-format / ordering drift. |
| `e2e` | Bounded run of the **real `cass` binary** (or `--robot` surface) asserting stdout/stderr/exit + structured-log contract. |
| `logs` | A structured proof-log/artifact manifest per `.12.3` proving the run happened and distinguishing real pass from timeout. |

**Mandatory-for-closure rule.** A bead that changes code or user-visible
behavior closes only when every `mandatory` proof for its family is green and
cited, with the proof artifacts from `.12.3`/`.12.6`. A **design/docs-only**
bead closes by citing the reviewed doc/golden and stating why runtime tests
are N/A. Live-diagnostic (`optional`) proofs never gate closure.

**Convention for the contract cores already landed under `src/search/`.**
Many epics seed a *pure contract/classifier core* (deterministic, `now_ms`
or signal inputs, snake_case wire forms) whose closure proof is `unit` +
`golden` JSON round-trip; the *surface wiring* into `lib.rs`/TUI is a
separate slice whose closure proof is `e2e` + `logs`.

---

## Epic 1 — Canonical derived-asset readiness and archive-risk control

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Derived-asset truth-table schema & readiness vocabulary | `unit` (enum wire forms, `safe_next_command` priority, fixture round-trip) + `golden` (fleet-state fixtures) | — |
| Archive-risk safety envelope (`unsafe_until`, backup-first) | `unit` (high-risk gates mutating actions; low-risk-stale keeps refresh) | — |
| Readiness wired into health/status/triage/search `_meta` | `e2e` (robot JSON includes readiness + recommended_action) + `logs` | `unit` projection |
| Archive-risk states surfaced in status/doctor/index-preflight | `e2e` (high-risk never emits casual rebuild advice) + `golden` JSON | — |

## Epic 2 — Quiet bounded robot commands and archive-capable view

| Family | Mandatory | Optional |
|--------|-----------|----------|
| stdout/stderr hygiene for robot commands | `e2e` (no stray stdout on `--robot`; logs to stderr only) + `golden` | — |
| Bounded execution budgets + partial/error envelopes | `unit` (budget/partial envelope) + `e2e` (timeout yields partial, not hang) | — |
| `cass view` resolves archive-only rows | `integration` (archive row read) + `e2e` | — |

## Epic 3 — Quarantine and ingest-OOM diagnosis, eligibility, and retry

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Quarantine status grouped by cause/version/eligibility | `unit` + `golden` | — |
| Bounded retry for eligible entries | `unit` (eligibility gate) + `integration` (no unbounded growth) | — |
| Compatibility/migration fixtures (legacy ↔ current) | `unit`/`integration` over real `QuarantineState::load` (roundtrip, malformed-safe, retry eligibility, dedup) + `golden` JSON fixtures | — |

## Epic 4 — Index, watch, and historical salvage liveness

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Universal progress/stall contract (heartbeat ≠ forward progress) | `unit` (building/stalled/stale/waiting_on_lock/ready) | — |
| Salvage ledger skips zero-new bundles | `unit` (skip/inspect decision, source-fingerprint change, legacy migration) | — |
| Watch OOM recovery decisions (no rebuild loop) | `unit` (bounded vs full-rebuild; loop guard) | — |
| Parseable watch-exit envelopes | `unit` (stable `err.kind`/retryability) + `e2e` (real `--watch` exit emits envelope) | — |
| Liveness regression fixtures | `unit` (no degraded fixture recommends unbounded waiting) + `logs` (deterministic, distinguishes pass from timeout) | — |

## Epic 5 — Semantic tier readiness, progress, and truthful hybrid fallback

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Semantic readiness reasons + tier state | `unit` (each reason; no model download) | — |
| Backfill progress sink enable/inspect | `unit` (ordering, required fields, failure events, best-effort write failure) + `golden` (pinned JSONL schema) | live `e2e` tail |
| Checkpoint resume + partial-publish safety | `unit` (never publish a tier that lies about DB coverage; conversation-safe resume) | — |
| Truthful hybrid fallback metadata | `e2e` (`--robot-meta` fallback_tier/reason matches reality) | — |
| Model acquisition checksum / air-gapped UX | `integration` (checksum mismatch path) | live download `e2e` |

## Epic 6 — Bounded fleet doctor and version harmonization

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Bounded fleet-doctor probe schema | `unit` + `golden` | — |
| Probe timeouts / unreachable-host handling | `unit` (bounded; unreachable surfaced) + `e2e` | — |
| Upgrade rehearsal + post-upgrade verification | `integration` | live fleet `e2e` |

## Epic 7 — Workspace/source-path mismatch and archive fallback ergonomics

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Zero-result workspace diagnosis + canonical suggestions | `unit` (exact/case/path/platform/basename; source_id; genuine no-match) | — |
| Source existence & archive provenance (classifier) | `unit` (5 cases: present/missing/remote/pruned/path-mapped) | — |
| Provenance fields in hit/pack field policies | `e2e` (minimal/summary/custom include redacted fields) + `golden` | — |
| Archive-first drill-down (view/expand/pack) | `unit` (resolution + `not-found`/`ambiguous-source` err.kind) + `e2e` (archive-only fixture returns content; missing → err.kind) | — |
| Moved-workspace / stale-source fixtures | `unit` (canonical suggestion + provenance per scenario) + `golden` redaction | — |

## Epic 8 — Remote source sync, auth fallback, and unreachable-host safety

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Transport-decision JSON (OpenSSH-first fallback) | `unit`/`integration` + `golden` | — |
| Auth/permission/host-key failure surfacing | `integration` (each failure → stable err.kind + hint) | live `e2e` |
| Unreachable-host safety (no destructive local action) | `unit` + `e2e` | — |
| Source-config & setup race diagnostics | `integration` (TOCTOU-safe) | — |

## Epic 9 — Dependency and host-pressure root-cause attribution

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Root-cause attribution taxonomy (families + confidence) | `unit` (stable families, `Unknown` fallback never omitted) + `golden` | — |
| Dependency pin ↔ upstream-fix correlation | `unit` + `golden` | — |
| Root-cause family projected into status/doctor/fleet | `e2e` + `golden` | — |

## Epic 10 — Native bounded incident mining over CASS/session history

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Incident category schema (id/signals/family/privacy/probe) | `unit` (stable ordering; unknown-id non-mapping; family/privacy assignments) + `golden` | — |
| Bounded candidate discovery (caps/progress/partial) | `unit` (caps + partial results) + `e2e` (bounded, never unbounded scan) + `logs` | — |
| Ranked top-session pointers + category breadth | `unit`/`integration` (canonical row identity, ranking, provenance, existence, exact argv, redaction) + `golden` + `e2e` (robot/human parity) + `logs` | — |
| Redaction provenance + privacy audit | `unit` (privacy tier enforced) + `golden` redaction | — |

## Epic 11 — Real-binary proof gates, regression corpus, and canonical workflow docs

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Real-binary proof gate | `e2e` (real `cass` binary) + `logs` | — |
| Regression corpus for mined issue classes | `integration`/`golden` (each issue class reproduced) | — |
| Robot docs / canonical workflow recipes | design/docs: cite reviewed `docs/ROBOT_MODE.md` + recipe goldens | — |
| Proof artifacts distinguish pass / timeout / stale | `logs` (manifest per `.12.3`) | — |
| Integrated golden + E2E gate for the full graph | `e2e` + `golden` + `logs` | — |

## Epic 12 — Comprehensive test scripts and proof logging

| Family | Mandatory | Optional |
|--------|-----------|----------|
| This test matrix + closure checklist | design/docs: this file, reviewed | — |
| Bounded E2E runner with structured logs | `integration` (runner) + `logs` | — |
| Proof logging schema + artifact manifest + retention | `unit` (schema) + `golden` | — |
| Per-workstream unit/E2E harness requirements | design/docs + cross-refs here | — |
| Report-derived E2E scenario scripts | `e2e` + `logs` | — |
| CI/local proof recipe + log-completeness gate | `e2e` (CI) + `logs` | — |

## Epic 13 — User-facing recovery journeys and human/robot guidance parity

| Family | Mandatory | Optional |
|--------|-----------|----------|
| End-to-end recovery journey scenarios | `e2e` + `logs` | — |
| Robot readiness mirrored in human CLI/TUI summaries | `e2e` (human/robot parity) + `golden` | — |
| Redacted recovery evidence bundle UX | `integration` (redaction) + `golden` | — |
| Human/robot parity E2E with detailed logs | `e2e` + `logs` | — |

## Epic 14 — Storage integrity, legacy DB interop, and concurrency-safe repair

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Storage-integrity diagnostic taxonomy + JSON contract | `unit` + `golden` | — |
| Backup-first storage salvage / repair planner | `unit` (backup-first; never destructive without backup) + `integration` | — |
| Concurrency / busy-lock / WAL sidecar / stale-cache diagnostics | `integration` (real frankensqlite, WAL mode) | — |
| Storage failure fixtures + real-binary E2E gate | `integration`/`golden` + `e2e` | — |

## Epic 15 — Peripheral subsystem resilience and user-visible surface regression coverage

| Family | Mandatory | Optional |
|--------|-----------|----------|
| Connector ingest diagnostics + per-provider fixtures | `unit`/`integration` (per provider) + `golden` | — |
| Daemon socket pidfile cache + stale-searcher recovery | `integration` | — |
| HTML export encryption + content-rendering regression | `integration` + `golden` | — |
| Bookmarks / analytics / bakeoff / pages / TUI surfaces | `integration` + `golden` (no silent drift) | — |
| Config data-dir test-isolation + auxiliary CLI safety | `unit`/`integration` (isolation: `CASS_IGNORE_SOURCES_CONFIG` + fake HOME/XDG) | — |
| Subsystem coverage matrix + closeout gate | design/docs: this matrix + per-file checklist | — |

---

## Closure checklist (apply to every bead)

A bead may be closed only when:

1. **Scope preserved** — no requested functionality dropped to make tests
   easier; partial work stays `in_progress` with an explicit scope note.
2. **Mandatory proofs green** — every `mandatory` proof for the bead's
   family above is green and named in the closure reason (test path + result
   count, e.g. `cargo test --lib search::X = N passed/0 failed`).
3. **Robot/human surfaces** — if the bead changes a robot or human surface,
   an `e2e` proof and structured `logs` (per `.12.3`) are cited; a `golden`
   pins the wire form.
4. **Lint/format gates** — `ubs` exits 0 (criticals triaged; `#[cfg(test)]`
   helper panics are acceptable) and `cargo clippy --all-targets -- -D
   warnings` is green crate-wide.
5. **Real-pass vs timeout** — proof artifacts make a real pass
   distinguishable from a hang/timeout (deterministic in-memory tests, or a
   `.12.3` manifest for bounded runs).
6. **Design/docs beads** — close by citing the reviewed doc/golden and
   stating explicitly why runtime tests are N/A; never close on prose-only
   implementation claims.

## Notes on the seeded contract cores

Epics 1, 3, 4, 5, 7, 9, 10 already have their pure contract/classifier cores
landed under `src/search/` (and `src/indexer/`, `src/root_cause_taxonomy.rs`)
with `unit` + `golden` proofs green. For each, the **surface-wiring slice**
(into `lib.rs` status/health/triage/search-meta/view/pack, or the TUI) is the
remaining work and carries the `e2e` + `logs` mandatory proofs in the table
above. This matrix is the authority for which of those slices still owe an
`e2e`/`logs` proof before their epic can be declared complete.
