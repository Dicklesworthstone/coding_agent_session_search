# Pass 6/10 - Error Mapping Local Helper

## Isomorphism Card

### Change

Extract private `resolve_env_var` in `src/pages/config_input.rs` for four repeated `dotenvy::var(env_var).map_err(|_| ConfigError::EnvVarNotFound(env_var.to_string()))` sites.

### Equivalence Contract

- Inputs covered: `encryption.password`, `deployment.output_dir`, `deployment.account_id`, and `deployment.api_token` values with `env:` prefixes.
- Ordering preserved: yes. Each field is still resolved in the same order.
- Error semantics: unchanged. Missing or unreadable env vars still become `ConfigError::EnvVarNotFound(env_var.to_string())`; the inner `dotenvy::Error` remains intentionally discarded.
- Laziness: unchanged. `env_var.to_string()` is still constructed only in the error path.
- Observable output: unchanged. `ConfigError` derives the same display string, `Environment variable not found: {name}`.
- Public API / schema: unchanged. Helper is private.

### Candidate Score

- LOC saved: 6
- Confidence: 5
- Risk: 1
- Score: 30.0
- Decision: accept. The error construction is identical and local to one config module.

## Files Changed

- `src/pages/config_input.rs`: added private `resolve_env_var` and replaced four repeated error-mapping callsites.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass6_config_env_error_mapping.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all four callsites after extraction.
- Confirmed each `strip_prefix("env:")` guard is unchanged and still supplies the same env var name to the helper.
- Confirmed `dotenvy::var`, not `std::env::var`, is still used.
- Confirmed no validation logic or field assignment moved across another field.

## Verification

- `rustfmt --edition 2024 --check src/pages/config_input.rs`
  - Result: passed with no output.
- `git diff --check -- src/pages/config_input.rs refactor/artifacts/20260425T024205Z-second-simplify/pass6_config_env_error_mapping.md`
  - Result: passed with no output.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --lib pages::config_input::`
  - Result: passed, `18 passed; 0 failed`.
