# Pass 6/10 - Option/Default Narrowing

## Isomorphism Card

### Change

Extracted the repeated token-usage agent SQL fallback into `token_usage_agent_sql_or_unknown(...)`.

### Equivalence Contract

- Fallback priority: unchanged. Use the discovered agent SQL when available.
- Missing-agent fallback: unchanged: SQL literal expression `'unknown'`.
- Call sites: unchanged behavior in token-daily-status fallback and Track B breakdown fallback.
- SQL shape: unchanged except the fallback expression now comes from one helper.
- Public API: unchanged; helper is private.

### Candidate Score

- LOC saved: small, but removes duplicated fallback policy in two raw token-usage paths.
- Confidence: 5
- Risk: 1
- Score: 2.5
- Decision: accept. The fallback is easy to prove and directly reduces drift risk.

## Files Changed

- `src/analytics/query.rs`: added `token_usage_agent_sql_or_unknown(...)` and used it at both fallback sites.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass6_token_usage_agent_fallback.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read both replaced `unwrap_or_else` call sites and confirmed they used the exact same fallback expression.
- Confirmed the helper consumes the `Option<String>` in the same place, so ownership and clone behavior at call sites are unchanged.
- Verified both a status-path test group and a Track B breakdown fallback test.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/query.rs`
- Passed: `git diff --check -- src/analytics/query.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass6_token_usage_agent_fallback.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::query::tests::query_status`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::query::tests::query_breakdown_by_model_uses_track_b`
