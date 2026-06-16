# Subsystem Coverage Matrix & Closeout Gate

Bead: `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.5`.

> Generated from `src/subsystem_coverage_matrix.rs`. Do not edit by hand —
> run `UPDATE_GOLDENS=1 cargo test --lib subsystem_coverage_matrix` to
> regenerate after changing the matrix, and review the diff.

This is the **executable** counterpart of
[`RESILIENCE_TEST_MATRIX.md`](RESILIENCE_TEST_MATRIX.md) (bead `.12.1`):
the same coverage encoded as data that a test checks against the real
repository. The closeout gate fails when any of the 15 report
subsystem failure-mode files has no owning bead, no mandatory proof
level, no proof artifact, no logging expectation, or cites a proof
artifact that does not exist on disk (the "only prose evidence"
failure). The integrated resilience gate (bead `.11.5`) consumes
`subsystem_coverage_matrix::matrix_gaps()` so subsystem coverage cannot
be silently skipped during closeout.

## Proof levels

| Level | Meaning |
|-------|---------|
| `unit` | In-crate `#[cfg(test)]` over pure logic; no I/O. |
| `integration` | Real types across modules / `tests/` with isolated data dirs. |
| `golden` | A pinned artifact a change must deliberately update. |
| `e2e` | A bounded run of the real `cass` binary. |
| `logs` | A structured proof-log/manifest distinguishing pass from timeout. |

## Redaction expectations

| Value | Meaning |
|-------|---------|
| `no-user-content` | Diagnostics are structured counts/kinds/paths only. |
| `local-only` | Content stays local; only kinds/counts may be shared. |
| `redact-required` | User content present; redact before any sharing/export. |

## Subsystems

### analytics

- **Owning beads:** .15.4, .9.4
- **Failure modes:** fm-analytics-rebuild-grouped-aggregate, fm-analytics-charts-saturating-zero
- **Mandatory proofs:** unit, golden
- **Proof artifacts:** `src/metric_integrity.rs`, `tests/analytics_cost_pricing_table_contract.rs`
- **Optional diagnostics:** live analytics rollup tail under doctor --fix
- **Fixture provenance:** synthetic usage-ledger rows; no real session content
- **Log expectation:** deterministic --lib result line; proof-log when wired into doctor rebuild
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --lib metric_integrity + analytics contract test path/count

### bookmarks

- **Owning beads:** .15.4
- **Failure modes:** fm-bookmarks-silent-row-decode, fm-bookmarks-import-non-atomic
- **Mandatory proofs:** unit, integration
- **Proof artifacts:** `src/bookmarks.rs`, `src/metric_integrity.rs`
- **Optional diagnostics:** —
- **Fixture provenance:** synthetic bookmark rows; labels may embed workspace paths
- **Log expectation:** deterministic --lib result line; no structured log required for pure core
- **Redaction:** local-only
- **Closure evidence:** cargo test --lib bookmarks result line

### cache

- **Owning beads:** .15.2
- **Failure modes:** fm-cache-tantivy-searcher-stale
- **Mandatory proofs:** integration
- **Proof artifacts:** `src/daemon_runtime_state.rs`, `tests/search_caching.rs`, `tests/regex_cache.rs`
- **Optional diagnostics:** live searcher generation/reload tail
- **Fixture provenance:** synthetic generation counters; no user content
- **Log expectation:** deterministic --lib result line for SearcherCacheOutcome
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --lib daemon_runtime_state + search_caching

### cli_robot

- **Owning beads:** .2.4, .11.1, .11.5, .12.6
- **Failure modes:** fm-cli-robot-schema-drift, fm-cli-exit-code-regression, fm-cli-golden-snapshot-drift
- **Mandatory proofs:** e2e, golden, logs
- **Proof artifacts:** `tests/e2e_robot_smoke_gate.rs`, `tests/cli_robot.rs`, `tests/cli_robot_log_hygiene.rs`
- **Optional diagnostics:** —
- **Fixture provenance:** isolated empty data dir; real cass binary dispatch
- **Log expectation:** PhaseTracker structured log + manifest per .12.3 (E2E_LOG=1)
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --test e2e_robot_smoke_gate + golden robot JSON diff

### connectors

- **Owning beads:** .15.1, .3.1, .14.1
- **Failure modes:** fm-connectors-jsonl-parse-error, fm-connectors-cursor-vscdb-locked, fm-connectors-chatgpt-encrypted-undecipherable, fm-connectors-amp-stem-prefix-unsafe, fm-indexer-aider-external-id-collision
- **Mandatory proofs:** unit, integration, golden
- **Proof artifacts:** `src/connector_ingest_diagnostics.rs`, `tests/connector_cursor.rs`, `tests/connector_chatgpt.rs`, `tests/connector_aider.rs`, `tests/connector_amp.rs`
- **Optional diagnostics:** per-provider live ingest probe
- **Fixture provenance:** per-provider synthetic session files; encrypted blob is non-decipherable by design
- **Log expectation:** deterministic --lib result line; per-provider conformance fixtures
- **Redaction:** local-only
- **Closure evidence:** cargo test --lib connector_ingest_diagnostics + per-provider conformance

### daemon

- **Owning beads:** .15.2, .2.2, .4.1
- **Failure modes:** fm-daemon-stale-pidfile-socket, fm-daemon-fd-leak-on-tryclone
- **Mandatory proofs:** integration
- **Proof artifacts:** `src/daemon_runtime_state.rs`, `tests/daemon_client_integration.rs`
- **Optional diagnostics:** live socket bind/stale-cleanup observation
- **Fixture provenance:** synthetic runtime artifacts; flock .spawnlock, no pidfile
- **Log expectation:** deterministic --lib result line for DaemonRuntimeState
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --lib daemon_runtime_state + daemon_client_integration

### html_export

- **Owning beads:** .15.3, .10.5, .12.3, .13.3
- **Failure modes:** fm-html-export-markdown-injection, fm-html-export-encryption-failure, fm-encryption-nonce-type-safety, fm-encryption-utf8-byte-slicing
- **Mandatory proofs:** integration, golden, e2e
- **Proof artifacts:** `tests/html_export_sanitization_security.rs`, `tests/html_export_integration.rs`, `tests/html_export_e2e.rs`
- **Optional diagnostics:** —
- **Fixture provenance:** synthetic malicious markdown/script inputs; encrypted body is ciphertext
- **Log expectation:** structured proof-log for the real-binary export run; encrypted bodies stay ciphertext
- **Redaction:** redact-required
- **Closure evidence:** cargo test --test html_export_sanitization_security + golden html_export diff

### indexer

- **Owning beads:** .1, .4, .11.2, .12.5, .14.4
- **Failure modes:** fm-indexer-stale-lexical-publish-backups, fm-indexer-tantivy-corrupt-or-stale, fm-indexer-fsvi-vector-orphan, fm-indexer-zero-results-regression, fm-indexer-edge-ngram-mismatch, fm-indexer-double-saturating-sub
- **Mandatory proofs:** integration, golden
- **Proof artifacts:** `tests/indexer_tantivy.rs`, `tests/atomic_swap_publish_crash_window.rs`, `src/search/regression_corpus.rs`
- **Optional diagnostics:** live publish/backup retention tail
- **Fixture provenance:** synthetic index trees + crash-window fixtures; SQLite is source of truth
- **Log expectation:** proof-log over publish/atomic-swap runs; deterministic regression-corpus replay
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --lib search::regression_corpus + atomic_swap_publish_crash_window

### models

- **Owning beads:** .5, .5.1, .5.5
- **Failure modes:** fm-models-fastembed-missing, fm-models-onnx-load-failure, fm-models-checksum-mismatch
- **Mandatory proofs:** integration
- **Proof artifacts:** `tests/cli_model_lifecycle_contract.rs`, `tests/e2e_analytics_models.rs`
- **Optional diagnostics:** live model download (opt-in; never CI-required)
- **Fixture provenance:** missing/mismatched model dirs; no network at test time
- **Log expectation:** deterministic contract test result line; live download is opt-in only
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --test cli_model_lifecycle_contract

### pages

- **Owning beads:** .15.4, .13.2, .13.4, .12.6
- **Failure modes:** fm-pages-render-parity-drift, fm-pages-export-sanitization
- **Mandatory proofs:** integration, golden
- **Proof artifacts:** `tests/pages_export_golden.rs`, `tests/pages_pipeline_e2e.rs`, `tests/pages_error_handling_e2e.rs`
- **Optional diagnostics:** CI-only browser E2E (never local)
- **Fixture provenance:** synthetic conversations; exported page content is sanitized
- **Log expectation:** golden page artifact diff; CI browser logs for render fidelity
- **Redaction:** redact-required
- **Closure evidence:** cargo test --test pages_export_golden result line + golden diff

### search

- **Owning beads:** .1, .4, .5, .7, .11.2, .15.2
- **Failure modes:** fm-search-rrf-fast-unwrap-panic, fm-search-regex-pipe-in-charclass
- **Mandatory proofs:** unit, integration, golden
- **Proof artifacts:** `src/search/regression_corpus.rs`, `tests/search_pipeline.rs`, `tests/spec_search_determinism.rs`
- **Optional diagnostics:** live semantic refinement tail
- **Fixture provenance:** synthetic corpus; query/results stay local
- **Log expectation:** deterministic regression-corpus + determinism spec result lines
- **Redaction:** local-only
- **Closure evidence:** cargo test --lib search::regression_corpus + spec_search_determinism

### sources

- **Owning beads:** .8, .8.1, .8.2, .8.4, .8.5
- **Failure modes:** fm-sources-rsync-not-on-path, fm-sources-toml-malformed, fm-sources-toctou-existence-race
- **Mandatory proofs:** unit, integration, golden
- **Proof artifacts:** `src/source_doctor_health.rs`, `tests/e2e_sources.rs`, `tests/setup_workflow.rs`
- **Optional diagnostics:** live SSH source probe (opt-in)
- **Fixture provenance:** synthetic sources.toml + mirror dirs; no SSH session opened at test time
- **Log expectation:** deterministic --lib result line; structured logs for e2e source flows
- **Redaction:** local-only
- **Closure evidence:** cargo test --lib source_doctor_health + e2e_sources

### storage

- **Owning beads:** .14, .14.1, .14.4, .9.4
- **Failure modes:** fm-storage-frankensqlite-openread-cursor, fm-storage-pragma-integrity-fail, fm-storage-wal-multiprocess-corruption, fm-storage-rusqlite-frankensqlite-incompat, fm-storage-schema-version-drift, fm-storage-busy-lock-timeout, fm-storage-stale-wal-orphan, fm-storage-sql-fmt-injection-risk
- **Mandatory proofs:** integration, golden, e2e
- **Proof artifacts:** `tests/e2e_storage_failure_fixture_gate.rs`, `tests/storage.rs`, `tests/storage_migration_safety.rs`
- **Optional diagnostics:** live integrity-check sweep
- **Fixture provenance:** deterministic raw-byte corrupt fixtures; DB preserved byte-identical
- **Log expectation:** structured proof-log over the real-binary storage-failure gate
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --test e2e_storage_failure_fixture_gate result line

### tui

- **Owning beads:** .15.4, .13.2, .13.4, .12.6
- **Failure modes:** fm-tui-render-parity-drift, fm-tui-human-robot-mismatch
- **Mandatory proofs:** integration, golden, e2e
- **Proof artifacts:** `tests/e2e_human_robot_parity_gate.rs`, `tests/tui_smoke.rs`, `tests/e2e_tui_smoke_flows.rs`
- **Optional diagnostics:** live TUI asciicast capture
- **Fixture provenance:** headless TUI fixtures; human summaries carry redacted fields verbatim
- **Log expectation:** structured proof-log over the human/robot parity gate
- **Redaction:** redact-required
- **Closure evidence:** cargo test --test e2e_human_robot_parity_gate result line

### update_check

- **Owning beads:** .15.4
- **Failure modes:** fm-update-shell-injection, fm-update-clock-rollback
- **Mandatory proofs:** unit
- **Proof artifacts:** `src/update_check.rs`, `src/metric_integrity.rs`
- **Optional diagnostics:** live update-channel probe (opt-in)
- **Fixture provenance:** synthetic version strings + clock values; no network at test time
- **Log expectation:** deterministic --lib result line for sanitized-arg and clock-rollback paths
- **Redaction:** no-user-content
- **Closure evidence:** cargo test --lib update_check result line

---

Precursor docs (asserted to exist by the gate): `docs/RESILIENCE_TEST_MATRIX.md`, `docs/PROOF_RECIPE.md`.
