# Pass 7/10 - Local Error Constructor Helper

## Isomorphism Card

### Change

Extracted the duplicated key-slot ID conversion and overflow error construction into private `key_slot_id_for_len(...)`.

### Equivalence Contract

- Slot assignment: unchanged. Slot IDs still equal the current `key_slots.len()` when it fits in `u8`.
- Overflow behavior: unchanged. Counts above 255 still return an anyhow error with the same message shape and underlying conversion error text.
- Password slot behavior: unchanged.
- Recovery slot behavior: unchanged.
- Public API: unchanged; helper is private.

### Candidate Score

- LOC saved: 8
- Confidence: 5
- Risk: 1
- Score: 40.0
- Decision: accept. This removes duplicated error construction across the two key-slot add paths.

## Files Changed

- `src/pages/encrypt.rs`: added `key_slot_id_for_len(...)`, used it from both slot-add methods, and pinned the overflow message.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass7_key_slot_error_helper.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read both removed closures and confirmed the helper uses the same `self.key_slots.len()` value and same `u8::try_from` conversion.
- Confirmed no password/recovery validation or key wrapping order changed.
- Added a focused overflow test that checks the last valid slot ID and exact error text for slot 256.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/encrypt.rs`
- Passed: `git diff --check -- src/pages/encrypt.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass7_key_slot_error_helper.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib pages::encrypt::tests::key_slot_id_for_len_rejects_overflow`
