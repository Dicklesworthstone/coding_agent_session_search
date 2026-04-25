# Pass 4 - SourceFilter Cycle Matrix

## Change

Replace four repeated one-case `SourceFilter::cycle()` tests with a single
table-driven transition test.

## Isomorphism Card

- Inputs covered: `All`, `Local`, `Remote`, and `SourceId("laptop")`.
- Ordering preserved: table order follows the previous individual tests.
- Tie-breaking: N/A.
- Error semantics: N/A.
- Laziness: unchanged.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: test-only refactor; production code unchanged.
- Type narrowing: N/A.

## Fresh-Eyes Prompt

`Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?`

## Fresh-Eyes Result

I re-read the table against the removed tests:

- `All` still cycles to `Local`.
- `Local` still cycles to `Remote`.
- `Remote` still cycles to `All`.
- `SourceId("laptop")` still cycles to `All`.

The full-cycle, idempotence, and no-SourceId-invariant tests remain separate.
No fix was needed after reread.

## Verification

- `rustfmt --edition 2024 --check src/sources/provenance.rs`
- `git diff --check -- src/sources/provenance.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib sources::provenance::tests::test_source_filter_cycle`

All passed.
