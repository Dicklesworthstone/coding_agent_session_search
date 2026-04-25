# Pass 5 - Master Key Backup Note Constant

## Change

Extract `MASTER_KEY_BACKUP_NOTE` for the fixed `master-key.json` note text used
by the bundle helper and its shape test.

## Isomorphism Card

- Inputs covered: fixed note text in `master_key_backup_json(...)`.
- Ordering preserved: JSON object key order unchanged.
- Tie-breaking: N/A.
- Error semantics: N/A.
- Laziness: unchanged.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: `master-key.json` note text is byte-identical.
- Type narrowing: N/A.

## Fresh-Eyes Prompt

`Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?`

## Fresh-Eyes Result

I compared the removed literal with `MASTER_KEY_BACKUP_NOTE`:

`This file contains the wrapped DEK. Keep it with your recovery secret.`

The helper and test now share that exact constant. No fix was needed after
reread.

## Verification

- `rustfmt --edition 2024 --check src/pages/bundle.rs`
- `git diff --check -- src/pages/bundle.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib pages::bundle::tests::test_master_key_backup_json_shape`

All passed.
