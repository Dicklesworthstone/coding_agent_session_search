# Pass 5 - SSH username fallback helper

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Option/Default Flow
- Scope: `src/sources/sync.rs`
- Verdict: PRODUCTIVE, with unrelated cargo-test blocker

## Change

Replaced an inline username-normalization closure in the SFTP sync path with
private helpers:

- `first_nonblank_username(...)`
- `env_username(...)`

## Isomorphism Card

Preserved behavior:

- Username priority remains `user@host`, SSH config user, `USER`, then
  `LOGNAME`.
- Blank and whitespace-only candidates are still ignored.
- Accepted candidates are still trimmed before use.
- No sentinel fallback username was added; failure still returns the existing
  "Unable to determine SSH username..." error.
- Environment lookups remain lazy: `USER` is checked only if host/config
  candidates fail, and `LOGNAME` is checked only if `USER` fails.

## Fresh-Eyes Review

Re-read the original chain and the new helpers. The helper is intentionally
limited to nonblank selection and trimming; it does not change host parsing,
SSH config lookup, hostname selection, port selection, or the existing error
message.

Yes, preservation was verified according to the skill by reading the priority
chain directly and adding a focused helper test for priority, trimming, and
blank skipping. The focused cargo test was attempted but blocked by unrelated
dirty work in `src/storage/sqlite.rs`.

## Verification

Passed:

- `rustfmt --edition 2024 --check src/sources/sync.rs`
- `git diff --check -- src/sources/sync.rs`

Blocked:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib sources::sync::tests::test_first_nonblank_username_priority_and_trimming`
- Blocker: pre-existing dirty `src/storage/sqlite.rs` fails to compile at line
  4241 with `error: expected token: ','`.

