# Pass 7 - Analytics CLI Error Helper

## Change
- Extracted `analytics_query_cli_error(...)` for repeated analytics query `CliError` construction.
- Replaced four duplicated constructors in analytics status, tokens, tools, and models paths.
- Added a focused test pinning the resulting CLI error shape for a representative analytics DB error.

## Fresh-Eyes Review
- Re-read every replaced constructor and confirmed the helper preserves:
  - `code: 9`
  - `kind: db-error`
  - `message: e.to_string()`
  - hint text: `Check that the analytics tables exist and are not corrupt.`
  - `retryable: false`
- Confirmed the pass did not change query execution order, JSON projection calls, or analytics stderr summaries.
- Fixed one rustfmt-reported same-file formatting drift in the `Commands::Upgrade` robot-mode arm so the touched-file formatting gate passes.

## Verification
- `rustfmt --edition 2024 --check src/lib.rs`
- `git diff --check -- src/lib.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib analytics_query_cli_error_tests::analytics_query_cli_error_preserves_shape`

## Verdict
PRODUCTIVE: removed repeated analytics CLI error construction while preserving the exact error contract.
