# Pass 9 - Mirror URL Rejection Matrix

## Target

- File: `src/search/model_download.rs`
- Mission: Test Matrix

## Change

Converted repeated mirror URL rejection assertions in
`test_normalize_mirror_base_url_rejects_invalid_values` into an explicit
`(input, expected_fragment)` matrix with per-case diagnostics.

## Isomorphism Card

- Inputs: unchanged `mirror.example`, `file:///tmp/mirror`, and
  `https://mirror.example/cache?trace=abc`.
- Expected fragments: unchanged `invalid mirror URL`,
  `unsupported URL scheme`, and `must not include query or fragment`.
- Failure mode: still uses `unwrap_err()` for each rejected value.
- Diagnostics: strengthened with the input, expected fragment, and actual
  message.

## Fresh-Eyes Check

Re-read the matrix rows against the removed assertions. Confirmed all three
inputs, expected fragments, and `unwrap_err()` behavior are preserved, while
the assertion message now identifies the failing row.

Yes: preservation was verified with the diff and the focused rejection test.

## Verification

- `rustfmt --edition 2024 --check src/search/model_download.rs`
- `git diff --check -- src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass9_mirror_url_rejection_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::model_download::tests::test_normalize_mirror_base_url_rejects_invalid_values -- --exact`
- `ubs src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass9_mirror_url_rejection_matrix.md` reported 0 critical issues.
