# Pass 5 - Query Token Count Matrix

- Mission: Query Token Matrix
- Files changed: `src/search/query.rs`
- Commit: pending

## Change

Converted four repeated `parse_boolean_query(...)`/`tokens.len()` assertions in `query_token_list_parses_small_queries` into an explicit `(query, expected_len)` matrix.

Fresh-eyes follow-up also removed two pre-existing `panic!` surfaces in the same touched test module:

- an ignored progressive-profile harness now records a refinement error and returns it through the existing `Result` path;
- `stress_very_long_single_term` now uses a `matches!` assertion over `tokens.first()` instead of indexing and panicking in the non-term branch.

## Isomorphism Check

- Production code unchanged.
- The same four query inputs remain covered:
  - `hello`
  - `hello world`
  - `hello AND world`
  - `hello world foo bar`
- The same expected token counts remain enforced: `1`, `2`, `3`, and `4`.
- Per-row diagnostics now include the query text on failure.
- The progressive harness still fails on refinement errors, but now with `bail!` through its existing `Result<()>` return.
- The long-term stress test still requires exactly one 10K-character `Term` token.

## Fresh-Eyes Review

Prompt applied: "Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with \"fresh eyes\" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?"

Yes. I re-read the matrix against the removed four blocks and verified the query strings and expected token counts are identical. UBS then surfaced two real pre-existing `panic!` sites in the modified file, so I fixed both and re-ran the focused tests. UBS still reports six "hardcoded secret" criticals in this file; I inspected the cited lines and they are false positives on `context_token` and local query-token variables, not credentials or literals.

## Verification

- `rustfmt --edition 2024 --check src/search/query.rs`
- `git diff --check -- src/search/query.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass5_query_token_count_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourteenth_simplify cargo test --lib search::query::tests::query_token_list_parses_small_queries -- --exact`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourteenth_simplify cargo test --lib search::query::tests::stress_very_long_single_term -- --exact`
- `ubs src/search/query.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass5_query_token_count_matrix.md` still exits nonzero on six inspected false-positive `token` secret findings in pre-existing code; no `panic!` criticals remain.
