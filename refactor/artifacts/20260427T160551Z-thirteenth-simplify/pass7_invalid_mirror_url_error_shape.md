# Pass 7 - Invalid Mirror URL Error Shape

## Target

- File: `src/search/model_download.rs`
- Mission: Local Error Shape

## Change

Extracted `invalid_mirror_url(...)` for repeated
`DownloadError::InvalidMirrorUrl { url, reason }` construction in
`normalize_mirror_base_url(...)`.

Added `test_invalid_mirror_url_helper_shape` to pin the helper's variant fields,
display text, and non-retryable classification.

## Isomorphism Card

- Empty input still reports the original `base_url` string, preserving whitespace
  behavior from the removed literal.
- Parse, scheme, host, query, and fragment errors still report the trimmed URL.
- Every reason string is unchanged.
- `DownloadError::InvalidMirrorUrl` display text remains governed by the same
  enum variant.
- Retryability remains `false` for invalid mirror URLs.

## Fresh-Eyes Check

Re-read each `normalize_mirror_base_url(...)` rejection branch against the
removed struct literals. Confirmed empty input still reports the original
`base_url`, all other branches still report `trimmed`, and every reason string
is byte-for-byte unchanged.

Yes: preservation was verified with the diff, the helper-shape test, and the
existing normalization rejection test.

## Verification

- `rustfmt --edition 2024 --check src/search/model_download.rs`
- `git diff --check -- src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass7_invalid_mirror_url_error_shape.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::model_download::tests::test_invalid_mirror_url_helper_shape -- --exact`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::model_download::tests::test_normalize_mirror_base_url_rejects_invalid_values -- --exact`
- `ubs src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass7_invalid_mirror_url_error_shape.md` reported 0 critical issues.
