# Pass 6 - Effective Settings Compiled Projection

## Target

- File: `src/search/policy.rs`
- Mission: Projection Helper

## Change

Extracted `compiled_default_setting(...)` for effective-settings rows whose
provenance is always `SettingSource::CompiledDefault` and whose `env_var` is
always absent.

Converted four call sites:

- `fast_tier_embedder`
- `reranker`
- `semantic_schema_version`
- `chunking_strategy_version`

## Isomorphism Card

- `name`: same string literal passed to the helper.
- `value`: same final policy field and same string conversion.
- `source`: fixed at `SettingSource::CompiledDefault`, matching the removed
  struct literals.
- `env_var`: fixed at `None`, matching the removed struct literals.
- Ordering: unchanged; every row is still pushed in the same sequence.

## Fresh-Eyes Check

Re-read every converted push site against the removed struct literals.
Confirmed the helper preserves the same name literals, exact value expressions,
`SettingSource::CompiledDefault`, absent `env_var`, and row ordering.

Yes: preservation was verified against both the diff and the focused
effective-settings tests.

## Verification

- `rustfmt --edition 2024 --check src/search/policy.rs`
- `git diff --check -- src/search/policy.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass6_effective_settings_compiled_projection.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::policy::tests::effective_settings_version_fields_always_compiled -- --exact`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib search::policy::tests::effective_settings_all_defaults -- --exact`
- `ubs src/search/policy.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass6_effective_settings_compiled_projection.md` reported 0 critical issues.

Note: the focused cargo test builds reported the pre-existing `franken_insert_message`
dead-code warning from `src/storage/sqlite.rs`, which is unrelated dirty peer work.
