# Pass 7 - Enum/String Census

- Run: `20260425T234742Z-seventh-simplify`
- Timestamp: `2026-04-26T00:18:29Z`
- Mission: Enum/String Census
- Files changed: `src/search/policy.rs`

## Change

Added `as_str()` helpers for `SemanticMode`, `ModelDownloadPolicy`, and `SettingSource`, then made their `Display` implementations delegate to those helpers.

## Isomorphism Card

- `SemanticMode` display strings remain `hybrid_preferred`, `lexical_only`, and `strict_semantic`.
- `ModelDownloadPolicy` display strings remain `opt_in`, `budget_gated`, and `automatic`.
- `SettingSource` display strings remain `compiled_default`, `config`, `environment`, and `cli`.
- Existing parse aliases are unchanged.
- Existing serde `rename_all = "snake_case"` annotations are unchanged.

## Fresh-Eyes Review

Re-read each new helper against the old `Display` match arms. The public strings are byte-identical and are now pinned by a shared display/as_str parity test.

## Verification

- `rustfmt --edition 2024 --check src/search/policy.rs`
- `git diff --check -- src/search/policy.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib search::policy::tests::`

## Verdict

PRODUCTIVE
