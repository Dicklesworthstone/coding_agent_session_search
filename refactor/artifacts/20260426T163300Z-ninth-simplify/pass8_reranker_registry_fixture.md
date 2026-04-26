# Pass 8 - Fixture Helper: reranker registry tests

## Change

Added a local `registry_fixture()` helper in `src/search/reranker_registry.rs` tests and reused it for tests that all constructed a `TempDir` plus `RerankerRegistry`.

## Score

- LOC saved: 4
- Confidence: 5
- Risk: 1
- Score: 20.0
- Verdict: PRODUCTIVE

## Isomorphism Card

- Inputs covered: registry lookup, availability, validation, bake-off, baseline, metadata, and missing-file tests.
- Ordering preserved: test execution order remains independent.
- Tie-breaking: unchanged.
- Error semantics: unchanged; registry uses the same temp directory path.
- Laziness: unchanged.
- Short-circuit eval: unchanged.
- Floating-point: N/A.
- RNG/hash order: unchanged.
- Observable side effects: tempdir lifetime is preserved by returning `TempDir` alongside the registry; missing-file checks still inspect the same directory.
- Type narrowing: unchanged.

## Fresh-Eyes Review

Re-read each converted test and confirmed the helper preserves the tempdir-backed registry construction. Tests that need the filesystem path keep the returned `TempDir`; tests that only need registry lookup keep `_tmp` alive for the full test scope.

## Verification

- Passed: `rustfmt --edition 2024 --check src/search/reranker_registry.rs`
- Passed: `git diff --check -- src/search/reranker_registry.rs .skill-loop-progress.md refactor/artifacts/20260426T163300Z-ninth-simplify/pass8_reranker_registry_fixture.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib search::reranker_registry::tests::`
