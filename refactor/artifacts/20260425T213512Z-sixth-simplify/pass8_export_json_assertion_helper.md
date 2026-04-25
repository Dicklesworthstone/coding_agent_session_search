# Pass 8 - Export JSON Assertion Helper

- Mission: Assertion Helper
- Score: 2.0
- Files changed: `src/export.rs`

## Change

The exported-hit JSON shape test now uses test-local `assert_json_field(...)` for repeated field equality assertions.

## Isomorphism Proof

- Every previously checked field remains checked: `title`, `agent`, `workspace`, `snippet`, `score`, `source_path`, `line_number`, `created_at`, `created_at_formatted`, and `content`.
- The expected values are unchanged.
- Missing fields now compare as `None` against `Some(expected)`, preserving failure on absence while adding the full projected JSON to diagnostics.
- The final object-length assertion remains unchanged.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read the helper and converted assertions. No bugs found. The focused projection test still passes and all checked keys are still present in the test body.

## Verification

- `rustfmt --edition 2024 --check src/export.rs`
- `git diff --check -- src/export.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib export::tests::test_export_hit_json_shape`
