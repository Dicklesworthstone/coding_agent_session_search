# Pass 7/10 - Test Fixture DRY Pass

## Candidate

`tests/metamorphic_stats.rs` repeats the same temporary stats fixture setup in
three tests:

- create a temporary HOME
- derive `CODEX_HOME`
- derive and create the `cass_data` directory
- install HOME/CODEX_HOME guards

Two seeded tests also repeat the same `cass index --full --data-dir <dir>`
command with identical environment variables.

## Isomorphism Card

### Change

Extract a local `StatsFixture` with fixture, environment, indexing, stats, and
session-seeding helpers in `tests/metamorphic_stats.rs`.

### Equivalence Contract

- **Inputs covered:** the same three metamorphic stats tests.
- **Test names preserved:** yes.
- **Assertions preserved:** yes; no assertion is removed or weakened.
- **Fixture paths:** unchanged relative layout: `<tmp>/.codex` and
  `<tmp>/cass_data`.
- **Filesystem side effects:** `cass_data` is still created before command
  execution; `CODEX_HOME` remains created only by the empty test or by seeded
  session writes.
- **Command behavior:** `cass index --full --data-dir <dir>` is invoked with
  the same `HOME`, `CODEX_HOME`, `CODING_AGENT_SEARCH_NO_UPDATE_PROMPT`, and
  `CASS_IGNORE_SOURCES_CONFIG` values.
- **Seeded sessions:** same dates, filenames, message content, and timestamps.
- **Environment restoration:** still guarded by `EnvGuard`; both guards now
  live in one tuple until test end.
- **Ordering:** unchanged; setup, seeding, indexing, stats capture, assertions
  stay in the same sequence.
- **Error semantics:** unchanged `unwrap`, `expect`, and assert-command failure
  behavior.
- **Production behavior:** not touched.

### Score

- LOC saved: 2 (`tests/metamorphic_stats.rs` net -15 lines)
- Confidence: 5
- Risk: 1
- Score: 10.0

## Rejected Candidates

- `tests/metamorphic_agent_detection.rs`: already has focused session seeding
  and scan helpers; no rule-of-three fixture setup inside the module.
- `src/model/conversation_packet.rs`: fixture builders are already centralized
  within the module; further extraction would mostly couple raw and canonical
  fixture semantics.
- `src/model/packet_audit.rs`: similar raw/canonical builders exist, but the
  audit tests intentionally mutate canonical variants and redaction content;
  collapsing more would risk hiding projection-specific differences.
- `tests/e2e_large_dataset.rs`: explicitly avoided for routine work.

## Verification Plan

- `rustfmt --edition 2024 --check tests/metamorphic_stats.rs`
- `git diff --check -- tests/metamorphic_stats.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass7_test_fixture_dry.md`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test --test metamorphic_stats`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`
