# Pass 5/10 - Constant Literal Audit

## Isomorphism Card

### Change

Introduced local `query_status` constants for the five analytics table names used by status health checks and `TableInfo` output.

### Equivalence Contract

- Table names: unchanged: `message_metrics`, `usage_hourly`, `usage_daily`, `token_usage`, `token_daily_stats`.
- Status table order: unchanged.
- Query source tables and fallback behavior: unchanged.
- JSON output: unchanged because `TableInfo.table` receives the same strings.
- Scope: constants are local to `query_status`; no public API or module-level contract changed.

### Candidate Score

- LOC saved: small, but removes a typo-prone duplicated literal set.
- Confidence: 5
- Risk: 1
- Score: 2.5
- Decision: accept. The status inventory now has one local source of table-name truth.

## Files Changed

- `src/analytics/query.rs`: added local status table-name constants and reused them for existence checks, stat queries, and result rows.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass5_status_table_constants.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read each replacement against the original literal and confirmed exact spelling.
- Confirmed the output table vector still uses the same order as before.
- Left column names and SQL aliases literal because their context-specific prefixes differ.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/query.rs`
- Passed: `git diff --check -- src/analytics/query.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass5_status_table_constants.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::query::tests::query_status`
