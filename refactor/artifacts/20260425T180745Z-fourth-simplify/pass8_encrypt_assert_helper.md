# Pass 8/10 - Assertion Helper Pass

## Isomorphism Card

### Change

Added test-only `assert_file_bytes(...)` for repeated decrypted-file byte comparisons in `src/pages/encrypt.rs`.

### Equivalence Contract

- Checked data: unchanged. Each call still reads the decrypted file and compares exact bytes to the expected slice.
- Failure semantics: still panics on read failure or byte mismatch.
- Diagnostics: improved with the file path on read and mismatch failures.
- Production code: unchanged.
- Test coverage: unchanged call sites, with the same expected data.

### Candidate Score

- LOC saved: small, but removes repeated read/compare assertions and improves failure output.
- Confidence: 5
- Risk: 1
- Score: 2.5
- Decision: accept. This is test-only and preserves exact byte assertions.

## Files Changed

- `src/pages/encrypt.rs`: added `assert_file_bytes(...)` and used it in roundtrip/decryption assertions.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass8_encrypt_assert_helper.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all five converted assertions and confirmed each still compares the same `decrypted_path` against the same `test_data`.
- Confirmed the helper uses `actual.as_slice()` so byte equality remains slice-to-slice and does not change expected ownership.
- Kept the helper inside `#[cfg(test)]` so production code is untouched.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/encrypt.rs`
- Passed: `git diff --check -- src/pages/encrypt.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass8_encrypt_assert_helper.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib pages::encrypt::tests::test_encryption_roundtrip`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib pages::encrypt::tests::test_multiple_key_slots`
