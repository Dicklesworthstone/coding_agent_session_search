# Pass 3 - Unencrypted Blocked Literal Family

- Mission: Literal Table.
- Files changed: `src/pages/confirmation.rs`.
- Change: pinned the unencrypted-export robot error kind, message, and suggestion strings in private constants and reused them in `robot_mode_blocked_error()`.
- Isomorphism proof: the JSON keys and exit code are unchanged, and the constants hold byte-identical copies of the prior public strings.
- Fresh-eyes check: expanded the unit test from two field checks to exact JSON equality, then re-read the helper to confirm no field was added, removed, or renamed.
- Verification:
  - `rustfmt --edition 2024 --check src/pages/confirmation.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib pages::confirmation::tests::test_robot_mode_blocked_error`

Verdict: PRODUCTIVE
