# Pass 3 - Export Hit JSON Helper

- Mission: Projection Helper Narrowing
- Score: 3.0
- Files changed: `src/export.rs`

## Change

The per-hit JSON shape built by `export_json(...)` now lives in private helper `export_hit_json(...)`. The top-level export envelope remains in `export_json(...)`.

## Isomorphism Proof

- Required hit keys remain `title`, `agent`, `workspace`, and truncated `snippet`.
- `score` is still present only when `include_score` is true, and non-finite values still serialize as `0.0`.
- `source_path` and `line_number` are still gated by `include_path`, with `line_number` present only when the hit has one.
- `created_at` and `created_at_formatted` still follow the same timestamp branch.
- `content` is still included only when `include_content` is true and content is non-empty.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read the extracted helper against the removed inline block. No bugs found. The focused test pins all keys emitted by a fully populated hit and checks the NaN score fallback.

## Verification

- `rustfmt --edition 2024 --check src/export.rs`
- `git diff --check -- src/export.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib export::tests::test_export_hit_json_shape`
