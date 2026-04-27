# Pass 6 - Export Hit Assertion Matrix

- Mission: Assertion Matrix.
- Files changed: `src/export.rs`.
- Change: converted the repeated `test_export_hit_json_shape` field assertions into a single `(key, expected)` table.
- Isomorphism proof: every key and expected `serde_json::Value` from the prior assertions is present exactly once, and the final object-length check still requires ten emitted fields.
- Fresh-eyes check: re-read the table against the removed assertion list and confirmed the helper still reports the failing key through `assert_json_field(...)`.
- Verification:
  - `rustfmt --edition 2024 --check src/export.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib export::tests::test_export_hit_json_shape`

Verdict: PRODUCTIVE
