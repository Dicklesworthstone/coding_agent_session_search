# Pass 4 - Projection Helper: export JSON envelope

## Change

Extracted `export_json_value(...)` from `export_json(...)` so the JSON export envelope has one named projection helper and can be tested with a fixed `exported_at` value.

## Score

- LOC saved: 1
- Confidence: 5
- Risk: 1
- Score: 5.0
- Verdict: PRODUCTIVE

## Isomorphism Card

- Inputs covered: JSON export with one search hit, query metadata, count, timestamp, and hit projection fields.
- Ordering preserved: hit order is still the original slice iteration order.
- Tie-breaking: N/A.
- Error semantics: unchanged; `export_json` still falls back to `"{}"` only if pretty serialization fails.
- Laziness: unchanged; hits are still collected before serialization.
- Short-circuit eval: unchanged.
- Floating-point: score projection still uses `export_hit_json` and its finite-score handling.
- RNG/hash order: unchanged; no hash iteration introduced.
- Observable side effects: unchanged; timestamp still captured once per `export_json` call and no logs/I/O were added.
- Type narrowing: unchanged Rust types and private helper boundary.

## Fresh-Eyes Review

Re-read the helper, caller, and exact-shape test after the focused test failed once. The failure was in the new proof expectation, not implementation: `sample_hit()` uses score `8.5`, so the expected payload was corrected from `0.95` to `8.5`. Confirmed the helper preserves query/count/timestamp/hit ordering, still captures the timestamp once in `export_json`, and delegates each hit to the existing `export_hit_json` path.

## Verification

- Passed: `rustfmt --edition 2024 --check src/export.rs`
- Passed: `git diff --check -- src/export.rs .skill-loop-progress.md refactor/artifacts/20260426T163300Z-ninth-simplify/pass4_export_json_payload.md`
- Failed then fixed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib export::tests::test_export_json` initially exposed the incorrect expected fixture score in the new test.
- Passed after fix: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib export::tests::test_export_json`
