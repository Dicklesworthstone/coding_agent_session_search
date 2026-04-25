# Pass 3 - Master Key Backup JSON Helper

## Change

Extract the `master-key.json` projection in `src/pages/bundle.rs` into
`master_key_backup_json(...)`, with `generated_at` passed in so the shape can be
tested deterministically.

## Isomorphism Card

- Inputs covered: `EncryptionConfig` export id, key slots, fixed note text, and
  generated timestamp.
- Ordering preserved: object key insertion order in the `json!` macro remains
  `export_id`, `key_slots`, `note`, `generated_at`.
- Tie-breaking: N/A.
- Error semantics: serialization and file-write error paths are unchanged.
- Laziness: unchanged.
- Short-circuit eval: unchanged.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: `master-key.json` fields and values are unchanged.
- Type narrowing: N/A.

## Fresh-Eyes Prompt

`Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?`

## Fresh-Eyes Result

I re-read the helper against the removed inline JSON:

- `export_id` still serializes from `enc_config.export_id`.
- `key_slots` still serializes from `enc_config.key_slots`.
- The warning note text is byte-identical.
- `generated_at` still comes from `Utc::now().to_rfc3339()` at the write callsite;
  the helper merely accepts it as an argument so the test can pin the shape.

The only issue found was rustfmt wanting the helper signature wrapped; that was
fixed before verification. The focused test passed.

## Verification

- `rustfmt --edition 2024 --check src/pages/bundle.rs`
- `git diff --check -- src/pages/bundle.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib pages::bundle::tests::test_master_key_backup_json_shape`

All passed.
