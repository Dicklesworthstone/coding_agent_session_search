# Pass 2 - SyncMethod String Helper

## Change

Add `SyncMethod::as_str()` and make `Display` call it, so the sync method
string spellings have one explicit helper.

## Isomorphism Card

- Inputs covered: every `SyncMethod` variant.
- Ordering preserved: table test covers variants in the previous assertion
  order; runtime matching remains one branch per variant.
- Tie-breaking: N/A.
- Error semantics: N/A.
- Laziness: unchanged.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: display output is unchanged.
- Type narrowing: enum variants and pattern matches are unchanged.

## Fresh-Eyes Prompt

`Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?`

## Fresh-Eyes Result

I re-read the new helper and the removed `Display` match:

- `Rsync` remains `rsync`.
- `WslRsync` remains `wsl-rsync`.
- `Scp` remains `scp`.
- `Sftp` remains `sftp`.

The test now checks both the helper and `Display`, so the helper cannot drift
silently from the public string contract. No fix was needed after reread.

## Verification

- `rustfmt --edition 2024 --check src/sources/sync.rs`
- `git diff --check -- src/sources/sync.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib sources::sync::tests::test_sync_method_display`

All passed.
