# Pass 5 - Option/Default Flow

- Run: `20260425T234742Z-seventh-simplify`
- Timestamp: `2026-04-26T00:11:19Z`
- Mission: Option/Default Flow
- Files changed: `src/search/query.rs`

## Change

Extracted `no_limit_budget_bytes(...)` from `compute_no_limit_result_cap_from(...)` to name the bytes-budget fallback chain used for no-limit search caps.

## Isomorphism Card

- A positive explicit hit-count override still wins first and is clamped to `[NO_LIMIT_RESULT_MIN, NO_LIMIT_RESULT_MAX]`.
- A positive bytes override still wins over available-memory sizing.
- Malformed, zero, or negative bytes values still fall through to the memory/floor fallback.
- Available-memory sizing still divides by `NO_LIMIT_RAM_DIVISOR` and clamps to `[NO_LIMIT_BYTES_FLOOR, NO_LIMIT_BYTES_CEILING]`.
- Missing available-memory data still falls back to `NO_LIMIT_BYTES_FLOOR`.
- Final hit-count conversion and clamp remain in `compute_no_limit_result_cap_from(...)`.

## Fresh-Eyes Review

Re-read the extracted helper and the caller against the removed inline chain. The helper preserves the exact priority order: hit-count env override, then bytes env override, then meminfo-derived budget, then floor.

## Verification

- `rustfmt --edition 2024 --check src/search/query.rs`
- `git diff --check -- src/search/query.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib no_limit`

## Verdict

PRODUCTIVE
