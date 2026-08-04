# Unit-Test Harness Requirements by Feature Family

Bead: `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.12.4`.

Many resilience fixes are schema, planner, classifier, resolver, or
state-machine work. Those defects must be caught by **fast unit tests that fail
close to the defect**, before the slower E2E scripts (`.12.2`) ever run. This
document specifies, for every feature family in the resilience rollout, the
unit-test harness requirements: **where** the tests live, **which categories**
are mandatory, and **which specific cases** must be covered — explicitly enough
that an implementer adding a fix knows exactly where and what to add.

It pairs with:

- **`.12.1`** — [`RESILIENCE_TEST_MATRIX.md`](RESILIENCE_TEST_MATRIX.md): the
  mandatory *proof level* (unit / integration / E2E / golden) per family for
  closure. This doc is the **unit-tier detail** behind that matrix.
- **`.12.2`** — the bounded E2E runner with structured logs (the slower tier
  these unit tests run ahead of).
- **`.12.3` / `.12.6`** — proof logging schema, artifact manifest, and the
  CI/local proof recipe + log-completeness gate.
- [`COVERAGE_POLICY.md`](COVERAGE_POLICY.md) — coverage targets and exclusions.
- [`planning/TESTING.md`](planning/TESTING.md) — the **no-mock policy** and
  fixture conventions all of the below inherit.

---

## Cross-cutting rules (apply to every family)

These are non-negotiable for **all** unit tests added under this spec:

1. **Four mandatory categories per unit.** Every classifier / resolver /
   state-machine / schema projection must have tests for:
   - **Happy path** — the expected, well-formed input → expected output.
   - **Negative path** — malformed / error / failure inputs are classified
     into the right explicit state (never silently swallowed or mis-bucketed).
   - **Boundary / empty** — empty collections, `None`/missing fields, zero
     counts, single-element, and the min/max of any numeric/version range.
   - **Report-derived regression** — at least one case reconstructed from a
     real report/incident finding, so a fixed defect cannot silently return.
2. **Fail close to the defect.** Prefer a pure function over the smallest unit
   (classifier, comparator, projection) so a failing assertion names the exact
   decision that broke — not a downstream symptom. Keep the live/networked I/O
   in the command layer and unit-test the pure core with fixtures.
3. **No mocks.** Per [`planning/TESTING.md`](planning/TESTING.md): real
   implementations + fixtures/test-doubles, never mock objects.
4. **frankensqlite only — no `rusqlite` in new tests.** New SQLite-touching
   tests MUST use `frankensqlite` (`fsqlite`) per AGENTS.md RULE 2. A new test
   that writes `use rusqlite` or `rusqlite::Connection` is a defect. (Existing
   rusqlite tests are legacy debt, not a pattern to copy.)
5. **Determinism.** No wall-clock / RNG dependence in assertions; pass
   timestamps/seeds in. Snake_case wire labels and `schema_version` values are
   pinned by an explicit round-trip/contract test.
6. **Robot-surface contract.** Any family that emits robot JSON pins its
   contract with a `serde` round-trip test plus assertions on stable field
   names and enum string values.

A bead in a family is **not unit-complete** until every row in its table below
has the four categories present (or an explicit, justified N/A).

---

## Feature families

### 1. Readiness truth table

- **Home:** `src/search/readiness.rs` (+ `src/search/readiness_fixtures.rs` for
  the canonical fixture matrix).
- **Happy:** each individual lexical / semantic / archive / quarantine /
  source-existence state classifies to its expected `SafeNextAction` /
  readiness verdict.
- **Negative / conflicting:** **every combination**, including conflicting
  states (e.g. stale-lexical + ready-semantic, archive-risk-high +
  index-present, source-missing + index-present). No combination may panic or
  fall through to a default "looks healthy".
- **Boundary / empty:** zero sessions, no lexical metadata, no semantic
  vectors, unknown/un-evaluated axes.
- **Regression:** the named fleet-host fixtures (`mac-mini-old` unreachable,
  `ts1` high-archive-risk, etc.) round-trip through JSON in stable order.

### 2. Command envelopes

- **Home:** `src/robot_budget_envelope.rs`, `src/topology_budget.rs`, and the
  robot-output paths in `src/lib.rs` (see [`ROBOT_MODE.md`](ROBOT_MODE.md),
  [`ERROR_CODES.md`](ERROR_CODES.md)).
- **Happy:** safe next command is classified safe; a complete result yields a
  non-partial envelope; `err.kind` strings match the stable taxonomy.
- **Negative:** unsafe next command is classified unsafe (never advertised as
  safe); timeout / partial-result produce the partial envelope with the right
  reason; invalid-JSON and command-failure are distinguishable.
- **Boundary / empty:** empty stdout, zero-length result set, exactly-at-budget
  vs. one-over-budget, ANSI on a non-TTY (must be stripped), stdout/stderr
  hygiene (no banner/log bleed into JSON stdout).
- **Regression:** `err.kind` stability — a contract test pins the full set of
  kind strings so a rename is caught.

### 3. Quarantine

- **Home:** `src/indexer/quarantine.rs`.
- **Happy:** records group correctly by cause / version / eligibility.
- **Negative:** retry-eligibility decisions for each cause; incomplete/missing
  metadata is handled (not assumed) and surfaced.
- **Boundary / empty:** empty quarantine set, single record, a cause with no
  retry path, `schema_version` at the legacy boundary.
- **Regression:** **migration from legacy quarantine records** produces the
  current grouping/eligibility without data loss.

### 4. Liveness / progress

- **Home:** `src/search/progress_contract.rs` (+ watch-mode error envelopes in
  `src/lib.rs`).
- **Happy:** heartbeat advances; a completed operation reports `complete`.
- **Negative:** **stalled vs. slow vs. complete** are distinguished (a slow but
  advancing op is not reported stalled); watch-mode errors are parseable
  (structured, not a bare string).
- **Boundary / empty:** zero-progress start, single tick, resume-from-checkpoint
  at 0% and at 100%, heartbeat exactly at the stall threshold.
- **Regression:** a recorded stall incident reproduces as `stalled`.

### 5. Semantic fallback

- **Home:** `src/search/` (semantic state / embedder-registry / model-download;
  see `src/search/model_download.rs`).
- **Happy:** model present + fresh vectors → semantic mode realized truthfully.
- **Negative:** model absent, model disabled, stale vectors, catch-up in
  progress, and **fingerprint mismatch** each fall back to lexical-only and the
  report states the *realized* mode truthfully (never claims semantic when it
  silently used lexical).
- **Boundary / empty:** zero vectors, partial vector index, exactly-stale
  boundary, empty corpus.
- **Regression:** a fingerprint-mismatch incident reproduces lexical fallback +
  truthful realized-mode reporting.

### 6. Fleet / source / remote

- **Home:** `src/fleet_doctor_schema.rs`, `src/source_doctor_health.rs`,
  `src/fleet_version_skew.rs`, `src/fleet_archive_coverage.rs`,
  `src/sources/sync.rs` (see [`RESILIENCE_TEST_MATRIX.md`](RESILIENCE_TEST_MATRIX.md)).
- **Happy:** reachable + current host → healthy; consistent mirror → no gap.
- **Negative:** host capability differences (macOS/Linux paths/tools),
  **unreachable / timed-out / auth-denied** hosts stay visible as explicit
  states (never dropped from summaries); stale source paths and **remote-pruned**
  sources preserve local evidence; **SSH/rsync → scp → ssh2/SFTP fallback
  selection** picks the OpenSSH-first transport and records the decision.
- **Boundary / empty:** no configured sources, archive-only host (drilldown),
  mirror ahead vs. behind by exactly one, additive remote mirror preservation
  (never `--delete` / prune / source-log mutation in any suggested command).
- **Regression:** the documented unreachable hosts (`mac-mini-old`,
  intermittent `ts2`/`mac-mini-max`) classify to their preserved states; a
  doctor run is asserted **mutation-free**.

### 7. Root-cause and incident mining

- **Home:** `src/root_cause_taxonomy.rs`, `src/crash_replay.rs`,
  `src/workflow_analytics.rs` (incident rollups).
- **Happy:** a known failure signature maps to its **deterministic** category.
- **Negative:** ambiguous/unknown signatures fall to an explicit
  `unknown`/`other` family (never a confident wrong guess); redaction removes
  secrets/PII from snippets.
- **Boundary / empty:** empty corpus, a single incident, a scan hitting the
  **bounded scan cap** (must report truncation, not silently stop), host
  rollups with one host.
- **Regression:** mined issue classes from the report reproduce their category
  and **privacy-safe snippet** redaction.

---

## Closure checklist (per bead)

Before closing any bead in a family above, confirm:

- [ ] The pure decision unit (classifier/resolver/projection) has all four
      categories: happy, negative, boundary/empty, report-derived regression.
- [ ] Robot-JSON output (if any) has a `serde` round-trip + stable
      field-name / enum-string contract test, with `schema_version` pinned.
- [ ] No new test introduces `rusqlite`; SQLite-touching tests use
      `frankensqlite`.
- [ ] No mocks; fixtures/test-doubles only.
- [ ] Assertions are deterministic (no wall-clock/RNG).
- [ ] The corresponding row in [`RESILIENCE_TEST_MATRIX.md`](RESILIENCE_TEST_MATRIX.md)
      is satisfied at its mandatory proof level, with E2E/golden proof produced
      via `.12.2`/`.12.6` where that matrix requires it.
