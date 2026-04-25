# Pass 7/10 - Constant/Env Literal Consolidation

## Isomorphism Card

### Change

Extract private Cloudflare env-var constants in `src/pages/deploy_cloudflare.rs` and use them for credential/API-base lookups plus wrangler environment injection.

### Equivalence Contract

- Inputs covered: `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_API_BASE_URL`, and `CF_API_BASE_URL`.
- Ordering preserved: yes. Config values still override env credentials; `CLOUDFLARE_API_BASE_URL` still precedes `CF_API_BASE_URL`.
- Tie-breaking: unchanged.
- Error semantics: unchanged. The same `dotenvy::var(...).ok()` and `or_else` behavior is used.
- Observable strings: unchanged. Human-facing prerequisite messages still contain the literal env names exactly as before.
- Public API / schema: unchanged. Constants are private to the module.

### Candidate Score

- LOC saved: 0
- Confidence: 5
- Risk: 1
- Score: 2.0
- Decision: accept. This removes repeated magic env names while preserving documented strings and lookup order.

## Files Changed

- `src/pages/deploy_cloudflare.rs`: added private env constants and replaced internal lookup/injection literals.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass7_cloudflare_env_constants.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read credential lookup in `check_prerequisites` and `deploy`.
- Confirmed config values still take priority over environment values.
- Confirmed wrangler env injection still uses the exact same variable names.
- Confirmed prerequisite messages were intentionally left as literals to avoid accidental wording churn.

## Verification

- `rustfmt --edition 2024 --check src/pages/deploy_cloudflare.rs`
  - Result: passed with no output.
- `git diff --check -- src/pages/deploy_cloudflare.rs refactor/artifacts/20260425T024205Z-second-simplify/pass7_cloudflare_env_constants.md`
  - Result: passed with no output.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --lib pages::deploy_cloudflare::`
  - Result: passed, `16 passed; 0 failed`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --test deploy_cloudflare`
  - Result: passed, `23 passed; 0 failed`.
