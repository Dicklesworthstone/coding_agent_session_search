# Pass 7 - Wrapper Collapse: pages export temp path

## Change

Removed the private one-call `unique_atomic_temp_path(...)` wrapper in `src/pages/export.rs` and inlined its call to `unique_atomic_sidecar_path(..., "tmp", "pages_export.db")`.

## Score

- LOC saved: 2
- Confidence: 5
- Risk: 1
- Score: 10.0
- Verdict: PRODUCTIVE

## Isomorphism Card

- Inputs covered: `ExportEngine::execute(...)` temp output path generation and pages export unit tests.
- Ordering preserved: unchanged; temp path is still computed before any export writes.
- Tie-breaking: unchanged nonce/timestamp generation remains in `unique_atomic_sidecar_path`.
- Error semantics: unchanged; path generation is infallible and all I/O error paths remain in the caller.
- Laziness: unchanged.
- Short-circuit eval: unchanged.
- Floating-point: N/A.
- RNG/hash order: unchanged; same atomic nonce source.
- Observable side effects: unchanged temp path prefix/suffix/fallback literals and filesystem write flow.
- Type narrowing: unchanged `PathBuf` value.

## Fresh-Eyes Review

Re-read the inlined call against the removed helper. Confirmed the path argument, `"tmp"` suffix, `"pages_export.db"` fallback name, atomic nonce/timestamp helper, and Windows-only backup helper are unchanged.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/export.rs`
- Passed: `git diff --check -- src/pages/export.rs .skill-loop-progress.md refactor/artifacts/20260426T163300Z-ninth-simplify/pass7_pages_export_temp_path_inline.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib pages::export::tests::`
