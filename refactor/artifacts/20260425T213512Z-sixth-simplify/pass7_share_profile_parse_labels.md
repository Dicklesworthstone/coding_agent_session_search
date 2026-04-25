# Pass 7 - ShareProfile Parse Labels

- Mission: Enum/String Census
- Score: 2.0
- Files changed: `src/pages/profiles.rs`

## Change

`ShareProfile::from_str(...)` now searches `ShareProfile::all()` by each profile's existing `label()` instead of maintaining a separate match table with the same strings.

## Isomorphism Proof

- Accepted labels remain `public`, `team`, `personal`, and `custom`.
- Input remains case-insensitive through `to_ascii_lowercase()`.
- Unknown inputs still return `Unknown profile: {input}` with the original spelling.
- Display formatting still uses the separate icon/name helpers and is unchanged.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read `label()`, `all()`, and the parser. No bugs found. Existing parse/display tests plus the new label parity test verify the parse spellings are preserved.

## Verification

- `rustfmt --edition 2024 --check src/pages/profiles.rs`
- `git diff --check -- src/pages/profiles.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib pages::profiles::tests::test_profile`
