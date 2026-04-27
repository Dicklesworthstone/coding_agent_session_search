# Pass 6 - Reranker Lookup Matrix

- Mission: Reranker Surface
- Files changed: `src/search/reranker_registry.rs`
- Commit: pending

## Change

Converted repeated `registry.get(...).is_some()` plus `unwrap().id` assertions in `test_registry_get_by_name` into an explicit `(name, expected_id)` matrix.

## Isomorphism Check

- Production code unchanged.
- `ms-marco` still resolves to `ms-marco-minilm-l6-v2`.
- `bge-reranker-v2` still resolves to `bge-reranker-v2-m3`.
- The `unknown` negative lookup remains unchanged.
- Failure diagnostics now include the queried name.

## Fresh-Eyes Review

Prompt applied: "Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with \"fresh eyes\" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?"

Yes. I re-read both rows against the removed assertions and verified each lookup name and expected ID is identical. No follow-up fix was needed.

## Verification

- `rustfmt --edition 2024 --check src/search/reranker_registry.rs`
- `git diff --check -- src/search/reranker_registry.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass6_reranker_lookup_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourteenth_simplify cargo test --lib search::reranker_registry::tests::test_registry_get_by_name -- --exact`
- `ubs src/search/reranker_registry.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass6_reranker_lookup_matrix.md` reported no critical issues.
