# Pass 1 - Rust Derive/Error Boilerplate

## Candidate

Replace the hand-written `DownloadError` `Display`, `Error`, and `From<std::io::Error>` implementations in `src/search/model_download.rs` with `thiserror::Error` derive annotations.

Score: LOC 2 x confidence 5 / risk 1 = 10.

## Isomorphism Card

### Equivalence contract
- Inputs covered: every `DownloadError` display variant, existing retry/temp-discard callers, and the existing model download unit test.
- Ordering preserved: N/A. The change only affects trait implementations for a single enum.
- Tie-breaking: N/A.
- Error semantics: same variant constructors, same retry classification, same `std::io::Error` source behavior for `IoError`, and no source for string/struct variants.
- Laziness: unchanged.
- Short-circuit eval: unchanged.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: no logs, metrics, DB writes, network, or filesystem effects are introduced.
- Type narrowing: enum variant names and fields are unchanged.
- Public text: every `Display` string is preserved exactly, including capitalization and punctuation.

### Verification
- Baseline: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_simplify_pass1 cargo test --lib test_download_error_display` - passed: 1 test passed, 0 failed, 4121 filtered out.
- Original after edit: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_simplify_pass1 cargo test --lib test_download_error_display` - passed: 1 test passed, 0 failed, 4121 filtered out.
- Proof tightening: `test_download_error_display` now has exact-display cases for `NetworkError`, `VerificationFailed`, `Cancelled`, `Timeout`, `HttpError`, both `ManifestNotVerified` branches, and `InvalidMirrorUrl`; `IoError` exact display, `From<std::io::Error>`, and source behavior are tested separately.
- Proof-tightened test: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fresh_pass1 cargo test --lib test_download_error_display` - passed: 1 test passed, 0 failed, 4121 filtered out.
- Rustfmt: `rustfmt --edition 2024 --check src/search/model_download.rs` - passed with no output.
- Cargo fmt note: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fresh_pass1 cargo fmt --check -- src/search/model_download.rs` checked unrelated crate files and failed on pre-existing formatting drift in `tests/golden_robot_docs.rs`, so the targeted file proof uses direct `rustfmt`.
- Fresh-eyes repair: the earlier table omitted `Cancelled`, `Timeout`, `HttpError`, and `InvalidMirrorUrl`; those exact strings are now pinned.

## Files Changed

- `src/search/model_download.rs`: replaced hand-written `Display`, `Error::source`, and `From<std::io::Error>` for `DownloadError` with `thiserror::Error` annotations; tightened the existing display test to exact string checks for every variant plus source/from behavior.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass1_derive_error_boilerplate.md`: this isomorphism card.

## LOC Ledger

- `src/search/model_download.rs`: 3124 lines before the derive pass, 3125 lines after proof tightening, delta +1 overall.
- Implementation diff after proof tightening: 81 insertions, 80 deletions in the touched Rust file.
- Note: the behavior implementation still removes the hand-written trait boilerplate; the net line increase comes from expanding the exact-display proof table.

## Rejected Candidates

- `src/lib.rs` `CliError` and analytics display boilerplate: file is currently dirty from peer work, so this pass avoided it.
- `src/daemon/protocol.rs` trait derives: explicitly excluded because a prior pass already handled it.
- `src/search/hash_embedder.rs` `Default`: not derivable because default construction needs `DEFAULT_DIMENSION` and a matching delegate.
- `src/pages/encrypt.rs` `AeadSourceError`: adjacent AES-GCM error behavior was handled in a prior completed pass and is not a clean new target here.
