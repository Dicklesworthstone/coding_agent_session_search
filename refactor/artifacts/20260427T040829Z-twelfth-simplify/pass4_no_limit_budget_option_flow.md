# Pass 4 - No-Limit Budget Option Flow

## Target

- File: `src/search/query.rs`
- Seam: `no_limit_budget_bytes`

## Simplification

Extracted `no_limit_available_memory_budget(...)` so the no-limit budget fallback chain reads as:

1. Positive explicit byte override.
2. Available-memory-derived budget.
3. Fixed floor.

## Isomorphism Card

- `CASS_SEARCH_NO_LIMIT_BYTES` parse and positive-value filter still take precedence.
- Available memory is still divided by `NO_LIMIT_RAM_DIVISOR` and clamped to `[NO_LIMIT_BYTES_FLOOR, NO_LIMIT_BYTES_CEILING]`.
- Missing, invalid, or zero byte overrides still fall through to available memory, then to `NO_LIMIT_BYTES_FLOOR`.
- The helper is private and pure; no CLI, JSON, storage, or search-result contract changed.

## Fresh-Eyes Review

Re-read the old nested `unwrap_or_else` chain against the new `or_else(...).unwrap_or(...)` chain and the existing fallback-priority test. The extraction preserves precedence and only names the MemAvailable budget calculation.

## Verification

- `rustfmt --edition 2024 --check src/search/query.rs`
- `git diff --check -- src/search/query.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib search::query::tests::no_limit_budget_bytes_preserves_fallback_priority`

## Verdict

PRODUCTIVE. The option flow is simpler to audit and the existing regression test verifies the preserved fallback priority.
