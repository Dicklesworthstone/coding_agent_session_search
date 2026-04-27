# Pass 10 - Final Rescan Retryable Error Matrix

## Target

- File: `src/search/model_download.rs`
- Mission: Final Rescan and Dashboard

## Change

After rescanning the touched areas, converted repeated retryability assertions in
`test_retryable_error_classification` into an explicit `(DownloadError,
expected_retryable)` matrix with per-case diagnostics.

## Isomorphism Card

- Cases preserved: network error, timeout, HTTP 503, HTTP 404, cancelled, and
  verification failure.
- Expected booleans preserved: the first three are retryable; the latter three
  are not.
- Error payload strings and HTTP status codes are unchanged.
- Production code is unchanged in this pass; only test assertion structure
  changed.

## Fresh-Eyes Check

Re-read the matrix rows against the removed assertions. Confirmed all six
`DownloadError` variants/payloads and expected retryability booleans are
unchanged, with stronger per-error diagnostics on failure.

Yes: preservation was verified with the diff, the focused retryability test, and
UBS.

## Verification

- `rustfmt --edition 2024 --check src/search/model_download.rs`
- `git diff --check -- src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass10_final_rescan_retryable_error_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::model_download::tests::test_retryable_error_classification -- --exact`
- `ubs src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass10_final_rescan_retryable_error_matrix.md` reported 0 critical issues.
