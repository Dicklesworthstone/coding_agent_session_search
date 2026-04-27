# Pass 2 - Export Hit Base Projection

- Mission: Projection Helper.
- Files changed: `src/export.rs`.
- Change: extracted `export_hit_base_json(...)` from `export_hit_json(...)` so the always-present JSON hit fields are projected in one private helper before optional score/path/timestamp/content fields are added.
- Isomorphism proof: the helper contains exactly the moved `title`, `agent`, `workspace`, and truncated `snippet` fields. The optional branches still execute in the original order and still write the same keys and fallback score.
- Fresh-eyes check: re-read the moved projection and the optional mutation branches after the edit; confirmed `include_score`, `include_path`, `created_at`, and `include_content` behavior is unchanged and the exact-shape tests still cover the output.
- Verification:
  - `rustfmt --edition 2024 --check src/export.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib export::tests::test_export`

Verdict: PRODUCTIVE
