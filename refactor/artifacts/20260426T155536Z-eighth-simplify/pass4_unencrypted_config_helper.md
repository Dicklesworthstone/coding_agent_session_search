# Pass 4 - Unencrypted bundle config projection helper

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Projection Shape Helper
- Scope: `src/pages/wizard.rs`
- Verdict: PRODUCTIVE

## Change

Extracted the inline unencrypted bundle `config.json` value into the private
`unencrypted_bundle_config(db_size)` helper.

## Isomorphism Card

Preserved behavior:

- The caller still computes `db_size` from the copied `payload/data.db` file.
- The generated JSON still has `encrypted`, `version`, `payload`, and `warning`
  top-level keys with the same values.
- The nested payload object still uses path `payload/data.db`, format `sqlite`,
  and the caller-provided `size_bytes`.
- The output file path and durable write call are unchanged.

## Fresh-Eyes Review

Re-read the export branch around the extraction and the new helper. The helper
only replaces the `serde_json::json!` construction; copying the DB, reading its
metadata, serializing pretty JSON, and writing `config.json` all remain in the
same order.

Yes, preservation was verified according to the skill: the new test asserts the
exact JSON shape and values.

## Verification

- `rustfmt --edition 2024 --check src/pages/wizard.rs`
- `git diff --check -- src/pages/wizard.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib pages::wizard::tests::unencrypted_bundle_config_shape`

