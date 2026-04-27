# Pass 8 - Model Verified Marker Temp Wrapper

## Target

- File: `src/search/model_download.rs`
- Mission: Wrapper Collapse

## Change

Removed the private one-call `unique_model_temp_path(...)` wrapper and inlined
its exact `unique_model_sidecar_path(&marker_path, "tmp", ".verified")` call in
`ModelDownloader::write_verified_marker(...)`.

## Isomorphism Card

- Staging helper: unchanged shared `unique_model_sidecar_path(...)`.
- Suffix: unchanged `"tmp"`.
- Fallback filename: unchanged `".verified"`.
- Call location: unchanged write/sync/replace flow in `write_verified_marker(...)`.
- Independent contract: none; the removed wrapper had exactly one call site.

## Fresh-Eyes Check

Re-read the removed wrapper body and the new call site. Confirmed the direct
call uses the same `marker_path`, suffix, and fallback filename, and the
surrounding file create/write/sync/replace/sync-parent sequence is unchanged.

Yes: preservation was verified with the diff, a no-remaining-symbol scan, and
the marker overwrite test.

## Verification

- `rustfmt --edition 2024 --check src/search/model_download.rs`
- `git diff --check -- src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass8_model_verified_marker_temp_wrapper.md`
- `rg -n "unique_model_temp_path" src/search/model_download.rs || true` returned no matches.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::model_download::tests::test_write_verified_marker_overwrites_existing_marker -- --exact`
- `ubs src/search/model_download.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass8_model_verified_marker_temp_wrapper.md` reported 0 critical issues.
