# Pass 2 - Embedder Assertion Matrix

## Change

Convert the repeated `display.contains(...)` assertions in `test_embedder_info` into a compact expectation matrix with explicit diagnostics.

## Score

| LOC saved | Confidence | Risk | Score |
|---:|---:|---:|---:|
| 1 | 5 | 1 | 5.0 |

## Equivalence Contract

- Inputs covered: `EmbedderInfo` built from the fastembed fixture.
- Ordering preserved: assertion order remains model id, kind label, dimension text.
- Tie-breaking: N/A.
- Error semantics: unchanged test failure, now with the missing expected token in the message.
- Laziness: N/A; same display string is evaluated once.
- Short-circuit eval: unchanged enough for test semantics; each expected token is still checked.
- Floating-point: N/A.
- RNG/hash order: N/A.
- Observable side effects: none.
- Type narrowing: N/A.

## Fresh-Eyes Review

I re-read the matrix against the removed assertions. The three exact expected substrings are unchanged: the static fastembed id, `semantic`, and `384`. The surrounding field assertions still independently pin `id`, `dimension`, and `is_semantic`.

## Verification

- Passed: `rustfmt --edition 2024 --check src/search/embedder.rs`
- Passed: `git diff --check -- src/search/embedder.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass2_embedder_display_matrix.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::embedder::tests::test_embedder_info -- --exact`
