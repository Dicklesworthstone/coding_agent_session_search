## Pass 4/10 - Constant Literal Consolidation

### Candidate accepted

`src/search/tantivy.rs` repeats the same positive `usize` environment-variable
reader in three production limit helpers:

- `tantivy_writer_parallelism_hint_for_available`
- `tantivy_add_batch_max_messages`
- `tantivy_add_batch_max_chars`
- `tantivy_prebuilt_add_batch_max_messages`

Two callsites also repeat the literal
`"CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES"`. The accepted change extracts private
environment-name constants plus one private helper for the repeated positive
`usize` parsing pipeline.

### Score

| Candidate | LOC | Confidence | Risk | Score | Decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Tantivy positive env `usize` helper and env-name constants | 2 | 5 | 1 | 10.0 | Accepted |

### Isomorphism card

- **Inputs covered:** The three existing Tantivy env knobs:
  `CASS_TANTIVY_MAX_WRITER_THREADS`, `CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES`,
  and `CASS_TANTIVY_ADD_BATCH_MAX_CHARS`.
- **Ordering preserved:** Yes. Each caller still reads the same environment
  variable at the same point in its function before computing the same fallback.
- **Tie-breaking:** N/A.
- **Error semantics:** Unchanged. Missing env vars, parse failures, and zero
  values still fall through to the same defaults through `Option::None`.
- **Laziness:** Unchanged. Fallback defaults remain behind `unwrap_or` or
  `unwrap_or_else`; the batch fallbacks still evaluate only when the env value
  is missing, invalid, or zero.
- **Short-circuit eval:** Unchanged. The parse pipeline remains
  `dotenvy::var(...).ok().and_then(|value| value.parse::<usize>().ok()).filter(|value| *value > 0)`.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side-effects:** Unchanged. No JSON, CLI text, logs, index writes,
  or filesystem operations are touched.
- **Type narrowing:** N/A.
- **Rerender behavior:** N/A.

### Rejected candidates

- `src/storage/sqlite.rs` repeated `"PRAGMA busy_timeout = 5000;"`: safe-looking
  literal, but the file is a high-risk storage boundary and a constant-only
  extraction would be line-neutral while touching DB-open paths.
- `src/main.rs` and `src/search/tantivy.rs` both define the 26-thread Tantivy
  default: rejected because it crosses the binary/library boundary instead of
  staying inside one bounded module.
- `src/pages/key_management.rs` repeated `32`-byte key lengths: rejected because
  changing array type signatures to a const generic would touch sensitive crypto
  code and many callsites for little simplification payoff.

### Verification plan

- `rustfmt --edition 2024 --check src/search/tantivy.rs`
- `git diff --check -- src/search/tantivy.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass4_constant_literal_consolidation.md`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test search::tantivy --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`

### Verification results

- `rustfmt --edition 2024 --check src/search/tantivy.rs` passed.
- `git diff --check -- src/search/tantivy.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass4_constant_literal_consolidation.md` passed.
- `cargo fmt --check` still fails on unrelated pre-existing test formatting drift in
  `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`,
  `tests/metamorphic_agent_detection.rs`, and `tests/metamorphic_stats.rs`.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test search::tantivy --lib`
  passed: 19 passed, 0 failed, 4101 filtered out.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`
  passed.
- Exact `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings`
  still fails on unrelated `src/search/query.rs:5859`
  (`clippy::assertions-on-constants`).
- Diagnostic clippy with only that unrelated lint allowed passed:
  `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants`.
- `ubs src/search/tantivy.rs` completed. Its formatter, clippy, cargo check,
  and test-build subchecks were clean; the scanner still reports pre-existing
  heuristic findings in the file's test module (`expect`, `panic!`, and assert
  inventory), unrelated to this pass.

### LOC ledger

- `src/search/tantivy.rs`: 2002 lines before, 1998 lines after.
- Diff numstat: 14 insertions, 18 deletions, net -4 lines.
