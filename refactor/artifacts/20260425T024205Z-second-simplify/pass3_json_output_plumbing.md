# Pass 3/10 - Repeated JSON Output Plumbing

## Isomorphism Card

### Change

Collapse the repeated pretty-JSON serialization and write operand in `SearchAssetSimulationHarness::write_artifacts` into a private `write_pretty_json_file` helper in the same test utility module.

### Equivalence Contract

- Inputs covered: `failpoint_markers`, `actor_traces`, and `summary()` values written by `write_artifacts`.
- Ordering preserved: yes. The helper receives the same value references and `serde_json::to_vec_pretty` preserves the same `Serialize` traversal order.
- Tie-breaking: unchanged / N/A.
- Error semantics: same `std::io::Result` surface. Serialization errors are still converted with `std::io::Error::other`; `fs::write` errors still propagate unchanged from `std::fs::write`.
- Laziness: unchanged. Each artifact is still serialized immediately before its corresponding write call.
- Short-circuit evaluation: unchanged. The writes still run in the same order and stop at the first `?` error.
- Floating-point: N/A.
- RNG / hash order: unchanged / N/A. Existing `BTreeMap` summary ordering remains unchanged.
- Observable side effects: identical file paths, bytes, and write order for `failpoints.json`, `actor-traces.json`, and `summary.json`.
- Type narrowing: N/A.
- Rerender behavior: N/A.

### Candidate Score

- LOC saved: 2
- Confidence: 5
- Risk: 1
- Score: 10.0
- Decision: accept. This is a single-file, private test-support helper with exact repeated serialization and write behavior.

## Baseline

- On this worker's arrival, `tests/util/search_asset_simulation.rs` was already dirty with the accepted helper extraction present. I treated that as shared live work and verified the diff instead of reverting or rewriting it.
- Prior artifact note recorded this baseline command before the helper extraction: `env CARGO_TARGET_DIR=/tmp/cass_pass3_target cargo test --test search_asset_simulation -- robot_style_demo_is_deterministic_and_persists_artifacts --exact --nocapture`.
- Recorded baseline result: passed, `1 passed; 0 failed; 64 filtered out`.

## Files Changed

- `tests/util/search_asset_simulation.rs`: three repeated `fs::write(..., serde_json::to_vec_pretty(...).map_err(std::io::Error::other)?)?` callsites now call private `write_pretty_json_file`.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass3_json_output_plumbing.md`: this proof/verification card.

## LOC Ledger

- `tests/util/search_asset_simulation.rs`: 10 insertions, 12 deletions, net -2 lines.

## Rejected Candidates

- `snapshot_json`: left unchanged because it intentionally panics with `expect(...)`, computes a digest from the serialized bytes, and records snapshot metadata. Collapsing it with the fallible artifact writer would change error behavior and obscure the digest contract.
- `src/lib.rs` robot JSON writers: rejected by scope constraint. Public robot output code is broad and byte/error behavior must remain externally stable.
- Connector fixture pretty JSON writers: rejected for this pass because many are test-local fixture builders with `unwrap()`/`?` semantics that differ by test.

## Verification

- `rustfmt --edition 2024 --check tests/util/search_asset_simulation.rs`
  - Result: passed with no output.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_pass3_json cargo test --test search_asset_simulation -- robot_style_demo_is_deterministic_and_persists_artifacts --exact --nocapture`
  - Result: passed, `1 passed; 0 failed; 64 filtered out`.
- `git diff --check -- tests/util/search_asset_simulation.rs refactor/artifacts/20260425T024205Z-second-simplify/pass3_json_output_plumbing.md`
  - Result: passed with no output for tracked diff paths. The artifact is untracked, so it is covered by the no-index check below.
- `git diff --no-index --check -- /dev/null refactor/artifacts/20260425T024205Z-second-simplify/pass3_json_output_plumbing.md`
  - Result: no whitespace diagnostics. Exit code was `1` because `git diff --no-index` reports a difference when comparing `/dev/null` to a real file.

## Fresh-Eyes Review

- Re-read the changed `write_artifacts` block and helper after the focused test passed.
- Confirmed exact bytes are preserved: the helper still writes the direct `Vec<u8>` returned by `serde_json::to_vec_pretty(value)`.
- Confirmed error mapping is preserved: serialization errors still pass through `map_err(std::io::Error::other)` and `fs::write` errors remain ordinary `std::io::Error` propagation.
- Confirmed write order and short-circuiting are preserved: phase log, failpoints, actor traces, then summary; each call still uses `?` before the next artifact write.
