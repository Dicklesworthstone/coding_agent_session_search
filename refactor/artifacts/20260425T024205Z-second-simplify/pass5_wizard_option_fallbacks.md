# Pass 5/10 - Option Fallback Tightening

## Isomorphism Card

### Change

Extract two private deployment fallback helpers in `src/pages/wizard.rs`:

- `deploy_site_dir()`: preserves `final_site_dir.clone().unwrap_or_else(|| output_dir.join("site"))`.
- `deploy_project_name()`: preserves `repo_name.clone().unwrap_or_else(|| "cass-archive".to_string())`.

### Equivalence Contract

- Inputs covered: Local, GitHub Pages, and Cloudflare Pages deployment branches in `step_deploy`.
- Ordering preserved: yes. Values are still resolved at the same branch-local points before their first use.
- Tie-breaking / fallback priority: unchanged. Explicit `final_site_dir` wins over `output_dir/site`; explicit `repo_name` wins over `"cass-archive"`.
- Error semantics: unchanged / N/A.
- Laziness: unchanged for the fallback paths. `output_dir.join("site")` and `"cass-archive".to_string()` are still evaluated only when the option is absent.
- Observable strings: unchanged. The default deploy project name remains exactly `cass-archive`.
- Public API / schema: unchanged. Helpers are private methods.

### Candidate Score

- LOC saved: 13
- Confidence: 5
- Risk: 1
- Score: 65.0
- Decision: accept. The repeated fallback chains are identical and private to the deploy step.

## Files Changed

- `src/pages/wizard.rs`: added private deployment fallback helpers and replaced five repeated fallback chains.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass5_wizard_option_fallbacks.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all five replacements in `step_deploy`.
- Confirmed the Local branch only needs `deploy_site_dir`.
- Confirmed GitHub Pages and Cloudflare Pages still resolve the deploy directory before constructing their deployer/config.
- Confirmed the default string stays `cass-archive`; no deployment URL or prompt text changed.
- Confirmed the fallback path remains `self.state.output_dir.join("site")`, not any pre-resolved or global path.

## Verification

- `rustfmt --edition 2024 --check src/pages/wizard.rs`
  - Result: passed with no output.
- `git diff --check -- src/pages/wizard.rs refactor/artifacts/20260425T024205Z-second-simplify/pass5_wizard_option_fallbacks.md`
  - Result: passed with no output.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --lib pages::wizard::`
  - Result: passed, `21 passed; 0 failed`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --test pages_wizard`
  - Result: passed, `80 passed; 0 failed`.
