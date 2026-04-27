# Pass 1 - Confirmation Flow Fixture

- Mission: Test Fixture Surface.
- Files changed: `src/pages/confirmation.rs`.
- Change: added `basic_flow_with(...)` in the confirmation tests so flow-validation cases can start from the same basic config while keeping each case's mutation local.
- Isomorphism proof: each converted test previously built `make_basic_config()`, mutated fields, then called `ConfirmationFlow::new(config)`. The helper performs the same sequence and returns the same flow.
- Fresh-eyes check: re-read the three converted tests after the edit; confirmed `has_secrets`, `is_remote_publish`/`target_domain`, and `has_recovery_key`/`recovery_key_phrase` remain explicit at the call sites.
- Verification:
  - `rustfmt --edition 2024 --check src/pages/confirmation.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib pages::confirmation::tests::`

Verdict: PRODUCTIVE
