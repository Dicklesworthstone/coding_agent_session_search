# Pass 3 - SyncSchedule literal constants

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Constant/Literal Pin
- Scope: `src/sources/config.rs`
- Verdict: PRODUCTIVE

## Change

Pinned the three `SyncSchedule` display spellings as private constants and
reused those constants in the `Display` implementation and display test.

## Isomorphism Card

Preserved behavior:

- `SyncSchedule::Manual.to_string()` remains `manual`.
- `SyncSchedule::Hourly.to_string()` remains `hourly`.
- `SyncSchedule::Daily.to_string()` remains `daily`.
- TOML serde remains governed by `#[serde(rename_all = "lowercase")]`, not by
  the new constants.

## Fresh-Eyes Review

Re-read the constants, `Display` implementation, and existing TOML roundtrip
test. The constants are private and exactly match the previous string literals.
Using `f.write_str(match self { ... })` removes repeated formatting calls while
preserving the displayed text.

Yes, preservation was verified according to the skill: exact display values and
an existing config serialization roundtrip were both exercised.

## Verification

- `rustfmt --edition 2024 --check src/sources/config.rs`
- `git diff --check -- src/sources/config.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib sources::config::tests::test_sync_schedule_display`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib sources::config::tests::test_config_serialization_roundtrip`

