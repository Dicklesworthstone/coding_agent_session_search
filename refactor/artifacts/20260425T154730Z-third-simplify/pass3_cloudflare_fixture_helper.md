# Pass 3/10 - Rule-of-3 Fixture Helper

## Isomorphism Card

### Change

Extract `temp_cloudflare_deployer()` in `tests/deploy_cloudflare.rs` for five repeated setup spans that created a `TempDir` and `CloudflareDeployer::default()`.

### Equivalence Contract

- Inputs covered: Cloudflare header, redirects, bundle-structure, and overwrite tests.
- Setup order: unchanged. `TempDir::new()` still happens before `CloudflareDeployer::default()`.
- Lifetimes: unchanged. The returned `TempDir` remains bound in each test, so temporary directories live through all path assertions.
- Test assertions: unchanged. No assertion text or expected files changed.
- Runtime side effects: unchanged. The helper only constructs the same local fixture values.

### Candidate Score

- LOC saved: 5
- Confidence: 5
- Risk: 1
- Score: 25.0
- Decision: accept. This is a same-module test fixture helper with five identical setup spans.

## Files Changed

- `tests/deploy_cloudflare.rs`: added `temp_cloudflare_deployer()` and used it in five tests.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass3_cloudflare_fixture_helper.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all five replacement sites to confirm both local names remained `temp` and `deployer`.
- Confirmed `TempDir` is returned by value and kept alive for the duration of each test.
- Confirmed error-path tests without temp directories were intentionally left alone.

## Verification

- Passed: `rustfmt --edition 2024 --check tests/deploy_cloudflare.rs`
- Passed: `git diff --check -- tests/deploy_cloudflare.rs refactor/artifacts/20260425T154730Z-third-simplify/pass3_cloudflare_fixture_helper.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --test deploy_cloudflare` (23 passed)
