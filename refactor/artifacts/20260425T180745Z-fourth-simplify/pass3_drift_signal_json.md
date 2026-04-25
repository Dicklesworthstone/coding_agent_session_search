# Pass 3/10 - JSON Shape Projection Helper

## Isomorphism Card

### Change

Extracted the inline drift-signal JSON object from `StatusResult::to_json()` into private `DriftSignal::to_json()`.

### Equivalence Contract

- Status JSON keys: unchanged.
- Drift signal object keys: unchanged: `signal`, `detail`, `severity`.
- Value types: unchanged; all three fields remain strings.
- Drift freshness fields: unchanged and still serialized by `StatusResult::to_json()`.
- Public structs: unchanged. No fields or derives changed.

### Candidate Score

- LOC saved: small, but removes inline projection noise from the status envelope serializer.
- Confidence: 5
- Risk: 1
- Score: 2.5
- Decision: accept. This isolates a repeated-style row projection while preserving the JSON contract.

## Files Changed

- `src/analytics/types.rs`: added private `DriftSignal::to_json()`, used it from `StatusResult::to_json()`, and pinned the object shape.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass3_drift_signal_json.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read the removed inline `serde_json::json!` object against `DriftSignal::to_json()` and confirmed identical keys and field sources.
- Confirmed `StatusResult::to_json()` still constructs `tables`, `coverage`, `drift`, and `recommended_action` in the same places.
- Added a shape test that verifies all three drift-signal fields and rejects accidental extra fields.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/types.rs`
- Passed: `git diff --check -- src/analytics/types.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass3_drift_signal_json.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::types::tests::drift_signal_to_json_shape`
