# Pass 10 - Final Rescan and Dashboard Change

## Mission

Re-scan changed surfaces and make one final Score >= 2.0 simplification or record convergence.

## Scope

- `src/pages/errors.rs`

## Change

Converted `DbError` from a hand-written `Display` implementation plus empty `std::error::Error` implementation to `thiserror::Error` derive attributes.

This mirrors the already-verified `DecryptError` simplification from pass 1 and removes another local boilerplate match without changing the public user-facing strings.

## Isomorphism Check

- Every user-facing display string is unchanged:
  - `CorruptDatabase(_)` -> `The database appears to be corrupted.`
  - `MissingTable(_)` -> `The archive is missing required data.`
  - `InvalidQuery(_)` -> `Your search could not be processed.`
  - `DatabaseLocked` -> `The database is currently in use by another process.`
  - `NoResults` -> `No results found.`
- Internal details carried by string variants remain excluded from display output.
- `std::error::Error::source()` remains `None` for every variant.
- Existing suggestion, log-message, and error-code behavior is unchanged.

## Fresh-Eyes Review

Re-read the new attributes against the removed `Display` match arms and checked that `InvalidQuery(_)` still hides raw SQL in display output. Kept the existing no-internal-details test and added exact display/source parity coverage so the behavior remains pinned.

## Verification

- `rustfmt --edition 2024 --check src/pages/errors.rs`
- `git diff --check -- src/pages/errors.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::errors::tests::test_db_error`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::errors::tests::`
- `rustfmt --edition 2024 --check src/pages/errors.rs src/update_check.rs src/pages/deploy_cloudflare.rs tests/deploy_cloudflare.rs src/search/query.rs src/search/fastembed_embedder.rs src/search/policy.rs src/pages/deploy_github.rs`
- `cargo fmt --check` was run and remains blocked only by pre-existing formatting drift in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, and `tests/metamorphic_agent_detection.rs`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo check --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo clippy --all-targets -- -D warnings`

## Verdict

PRODUCTIVE.
