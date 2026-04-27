# Pass 4 - Protocol Error Display Matrix

- Mission: Daemon Error Shape
- Files changed: `src/daemon/protocol.rs`
- Commit: pending

## Change

Converted the repeated display/source assertions for `EncodeError` and `DecodeError` into a single `(label, error, expected_display)` matrix.

## Isomorphism Check

- Production code unchanged.
- `EncodeError("bad payload")` still must display as `encode error: bad payload`.
- `DecodeError("bad frame")` still must display as `decode error: bad frame`.
- Both error types still assert `source().is_none()`.
- Per-row diagnostics keep failures attributable to `encode` or `decode`.

## Fresh-Eyes Review

Prompt applied: "Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with \"fresh eyes\" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?"

Yes. I re-read the new trait-object table against the two removed assertion pairs and verified the display strings and `source().is_none()` checks are identical. No follow-up fix was needed.

## Verification

- `rustfmt --edition 2024 --check src/daemon/protocol.rs`
- `git diff --check -- src/daemon/protocol.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass4_protocol_error_display_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourteenth_simplify cargo test --lib daemon::protocol::tests::test_protocol_error_display_strings_are_preserved -- --exact`
- `ubs src/daemon/protocol.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass4_protocol_error_display_matrix.md` reported no critical issues.
