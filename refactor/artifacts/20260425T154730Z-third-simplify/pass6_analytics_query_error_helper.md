# Pass 6/10 - Error Mapping Helper

## Isomorphism Card

### Change

Extract `analytics_query_error(...)` in `src/analytics/query.rs` for repeated `AnalyticsError::Db(format!("{context}: {err}"))` query-error mappings.

### Equivalence Contract

- Error variant: unchanged. Every converted site still returns `AnalyticsError::Db`.
- Error text: unchanged. The helper inserts the same colon-space separator, preserving strings like `Analytics query failed: ...`.
- Query control flow: unchanged. Each `map_err` remains at the same fallible query boundary with the same `?` propagation.
- Scope limit: the one `AnalyticsError::Db(e.to_string())` mapping is intentionally unchanged because it has no context prefix.
- Public output: unchanged. Analytics result schemas and query rows are untouched.

### Candidate Score

- Repeated error mappings collapsed: 14
- Confidence: 5
- Risk: 1
- Score: 70.0
- Decision: accept. This is a local error-formatting helper with identical variant and message construction.

## Files Changed

- `src/analytics/query.rs`: extracted the query-error formatter and replaced context-prefixed duplicate closures.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass6_analytics_query_error_helper.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read each converted `map_err` and confirmed the context string exactly matches the old text before the colon.
- Confirmed both expression-ending forms were preserved: `)?` sites still return collected values, and `)?;` sites still bind intermediate rows.
- Confirmed the plain `AnalyticsError::Db(e.to_string())` mapping was not converted because adding a context would change the error message.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/query.rs`
- Passed: `git diff --check -- src/analytics/query.rs refactor/artifacts/20260425T154730Z-third-simplify/pass6_analytics_query_error_helper.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib analytics::query::` (121 passed)
