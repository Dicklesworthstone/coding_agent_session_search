# Pass 8 - Pages Config Password Fixture

- Mission: Fixture Builder.
- Files changed: `src/pages/config_input.rs`.
- Change: added `config_with_password()` for tests whose validation target is not missing-password behavior.
- Isomorphism proof: converted tests still start from `PagesConfig::default()` with `encryption.password = Some("test123")`; tests that intentionally cover missing password or no-encryption behavior remain explicit.
- Fresh-eyes check: re-read every converted test and confirmed the varied fields (`target`, `repo`, `chunk_size`, `path_mode`, Cloudflare credentials) are still set in the test body.
- Verification:
  - `rustfmt --edition 2024 --check src/pages/config_input.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib pages::config_input::tests::`

Verdict: PRODUCTIVE
