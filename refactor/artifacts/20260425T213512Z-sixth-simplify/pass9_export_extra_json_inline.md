# Pass 9 - Export Extra JSON Wrapper Hop Collapse

- Run: `20260425T213512Z-sixth-simplify`
- Mission: Wrapper Hop Collapse
- Target file: `src/pages/export.rs`
- Score: 2.0

## Change

Removed the private `parse_message_extra_json(...)` wrapper and inlined its exact body at the two adjacent callsites:

- `derive_message_model(...)`
- `derive_attachment_refs(...)`

The replacement expression is byte-for-byte equivalent in behavior:

```rust
let value: Value = serde_json::from_str(extra_json?).ok()?;
```

## Isomorphism Card

- `None` `extra_json` still returns `None` before parsing.
- Invalid JSON still maps to `None` through `.ok()?`.
- Valid JSON still produces a `serde_json::Value` used by the same pointer search tables.
- Model derivation order and blank-string filtering are unchanged.
- Attachment-ref derivation order, null suppression, and JSON serialization are unchanged.

## Fresh-Eyes Review

Re-read the removed wrapper and both replacement sites after the edit. No bug was found: the helper had no independent error mapping, logging, normalization, or contract beyond the parse expression, and the explicit `Value` type preserves the same owned JSON value used by the existing pointer calls.

## Verification

- `rustfmt --edition 2024 --check src/pages/export.rs`
- `git diff --check -- src/pages/export.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --test pages_export test_export_derives_model_from_extra_json_when_column_missing`

## Verdict

PRODUCTIVE
