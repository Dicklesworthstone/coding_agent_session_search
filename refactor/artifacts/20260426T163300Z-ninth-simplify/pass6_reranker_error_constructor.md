# Pass 6 - Error Constructor: reranker registry failures

## Change

Centralized `RerankerError::RerankFailed` construction in `src/search/reranker_registry.rs` with a local `rerank_failed(...)` helper.

## Score

- LOC saved: 2
- Confidence: 5
- Risk: 1
- Score: 10.0
- Verdict: PRODUCTIVE

## Isomorphism Card

- Inputs covered: unknown reranker, no available reranker, missing model directory, unimplemented reranker name, direct helper display/source parity.
- Ordering preserved: registry lookup and availability checks are unchanged.
- Tie-breaking: unchanged.
- Error semantics: same `RerankerError::RerankFailed` variant, same model string, same source display text, and source remains available via `Error::source`.
- Laziness: unchanged closures still construct errors only on failure paths.
- Short-circuit eval: unchanged `ok_or_else` behavior.
- Floating-point: N/A.
- RNG/hash order: unchanged.
- Observable side effects: unchanged; no logs/I/O/DB writes.
- Type narrowing: unchanged result types.

## Fresh-Eyes Review

Re-read each replacement against the original struct literals after formatting. Confirmed all model names are still taken from the same local variable/literal, source strings are unchanged, `ok_or_else` laziness is preserved, and the helper keeps `Error::source()` populated.

## Verification

- Passed: `rustfmt --edition 2024 --check src/search/reranker_registry.rs`
- Passed: `git diff --check -- src/search/reranker_registry.rs .skill-loop-progress.md refactor/artifacts/20260426T163300Z-ninth-simplify/pass6_reranker_error_constructor.md`
- Command-shape mistake: a two-filter cargo command failed before compiling because cargo accepts one name filter.
- Passed rerun: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib search::reranker_registry::tests::`
