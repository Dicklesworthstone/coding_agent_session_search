# Pass 1 - Semantic Env Fallback

## Change

Reuse the existing semantic `env_truthy(...)` parser in `parallel_prep_enabled()` instead of carrying a second inline copy of the same trim/lowercase/accepted-values chain.

## Score

| LOC saved | Confidence | Risk | Score |
|---:|---:|---:|---:|
| 2 | 5 | 1 | 10.0 |

## Equivalence Contract

- Inputs covered: missing `CASS_SEMANTIC_PREP_PARALLEL`; values `1`, `true`, ` YeS `, `on`, `0`, `false`, `off`.
- Ordering preserved: N/A; single environment lookup and pure parse.
- Tie-breaking: N/A.
- Error semantics: unchanged; missing or unrecognized values return `false`.
- Laziness: unchanged; one `dotenvy::var(...)` lookup.
- Short-circuit eval: unchanged.
- Floating-point: N/A.
- RNG/hash order: N/A.
- Observable side effects: unchanged; no writes in production code.
- Type narrowing: N/A.

## Fresh-Eyes Review

I re-read `parallel_prep_enabled()` against `env_truthy(...)`. Both implementations trim, lowercase, accept exactly `1`, `true`, `yes`, and `on`, and default to `false` when the environment variable is absent or unrecognized. The new `EnvVarGuard` test helper restores mutated variables on drop and also simplifies the existing default batch-size env test.

## Verification

- Passed: `rustfmt --edition 2024 --check src/indexer/semantic.rs`
- Passed: `git diff --check -- src/indexer/semantic.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass1_semantic_truthy_env.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib indexer::semantic::tests::parallel_prep_enabled_reuses_truthy_env_parser -- --exact`
