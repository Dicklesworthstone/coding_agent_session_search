# Pass 5 - Stale Config Env Helper

## Mission

Extract one option/default/env chain while preserving precedence.

## Change

Added `env_u64(...)` for the repeated numeric environment-variable pattern in `StaleConfig::from_env()`.

The three numeric settings now share the same missing/invalid fallback logic:

- `CASS_WATCH_STALE_THRESHOLD_HOURS`
- `CASS_WATCH_STALE_CHECK_INTERVAL_MINS`
- `CASS_WATCH_STALE_MIN_ZERO_SCANS`

## Isomorphism Card

- Inputs covered: missing environment variables, parseable numeric strings, and invalid strings by code-path equivalence.
- Ordering preserved: threshold, action, interval, and zero-scan overrides are still applied in the same order.
- Tie-breaking: unchanged; later assignments do not overlap.
- Error semantics: unchanged; missing or invalid numeric variables are ignored.
- Laziness: unchanged; each variable is read only when its branch is reached.
- Short-circuit eval: preserved by `ok()?` and `parse().ok()`.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side effects: unchanged; the helper reads the same environment keys and does not mutate process state.
- Robot JSON / public contracts: unchanged.

## Fresh-Eyes Review

Compared each old branch to `env_u64(...)`:

- Old `dotenvy::var(...).ok` failure -> no assignment; helper returns `None`.
- Old `val.parse()` failure -> no assignment; helper returns `None`.
- Old successful parse -> same `u64` assigned to the same field.
- `CASS_WATCH_STALE_ACTION` remains on its original string parser and fallback behavior.

No bug or semantic drift found.

## Verification

- `rustfmt --edition 2024 --check src/indexer/mod.rs`
- `git diff --check -- src/indexer/mod.rs .skill-loop-progress.md refactor/artifacts/20260426T210630Z-tenth-simplify/pass5_stale_config_env_helper.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_tenth_simplify cargo test --lib stale_config`

## LOC Delta

- `src/indexer/mod.rs`: 7 insertions, 9 deletions.
- Net: -2 lines.

## Verdict

PRODUCTIVE. The pass removed repeated numeric env parsing while preserving default precedence.
