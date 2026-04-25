# Pass 4/10 - Constant Literal Consolidation

## Isomorphism Card

### Change

Extract private `CASS_VERSION` in `src/pages/docs.rs` for three repeated `env!("CARGO_PKG_VERSION")` uses in generated README, SECURITY, and about text.

### Equivalence Contract

- Inputs covered: documentation generation for README.md, SECURITY.md, and about.txt.
- Replacement text: unchanged. `CASS_VERSION` is exactly `env!("CARGO_PKG_VERSION")`.
- Timing: unchanged. `env!` is compile-time; moving it to a `const` does not defer or alter lookup.
- Public API / schema: unchanged. Only private implementation literals changed.
- Runtime side effects: unchanged. Generated content receives the same version string.

### Candidate Score

- LOC saved: 2
- Confidence: 5
- Risk: 1
- Score: 10.0
- Decision: accept. This removes repeated compile-time version literals while preserving output.

## Files Changed

- `src/pages/docs.rs`: added private `CASS_VERSION` and used it in three generated docs.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass4_docs_version_constant.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all three replacement sites and confirmed only the version replacement changed.
- Confirmed date generation still happens at each callsite and was not accidentally centralized.
- Confirmed no public template placeholder or filename changed.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/docs.rs`
- Passed: `git diff --check -- src/pages/docs.rs refactor/artifacts/20260425T154730Z-third-simplify/pass4_docs_version_constant.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib pages::docs::` (11 passed)
