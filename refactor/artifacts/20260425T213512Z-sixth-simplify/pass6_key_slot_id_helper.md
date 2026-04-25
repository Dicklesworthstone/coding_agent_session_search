# Pass 6 - Key Slot ID Helper

- Mission: Error Mapping Helper
- Score: 3.0
- Files changed: `src/pages/key_management.rs`

## Change

Password and recovery key-slot addition now share `next_key_slot_id(...)` for stable max-plus-one ID allocation and overflow error construction.

## Isomorphism Proof

- Empty key-slot lists still allocate slot `0`.
- Non-empty key-slot lists still allocate `max(existing id) + 1`, preserving the no-reuse-after-revoke behavior.
- Overflow at slot ID `255` still returns `Cannot add more key slots: maximum slot ID (255) reached`.
- Password and recovery add flows still pass the computed slot ID into their original slot-creation functions.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read both callsites plus the helper. No bugs found. Existing `test_key_add_after_revoke_no_id_collision` still pins max-plus-one after a gap, and the new test pins the shared overflow message.

## Verification

- `rustfmt --edition 2024 --check src/pages/key_management.rs`
- `git diff --check -- src/pages/key_management.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib pages::key_management::tests::test_next_key_slot_id_rejects_max_id`
