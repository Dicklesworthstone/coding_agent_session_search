# Pass 1 - SizeError derive parity

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Display/Boilerplate Helper
- Scope: `src/pages/size.rs`
- Verdict: PRODUCTIVE

## Change

Converted `SizeError` from a hand-written `Display` implementation plus empty
`std::error::Error` implementation to `thiserror::Error` derive attributes.

## Isomorphism Card

Preserved behavior:

- `TotalExceedsLimit` still renders the same total-size sentence, blank line,
  `Suggestions:` header, and three suggestion bullets.
- `FileExceedsLimit` still renders the path, actual size, and limit size with
  the same `format_bytes(...)` formatting.
- `std::error::Error::source()` still returns `None` for every variant.
- `SizeError` still derives `Debug` and `Clone`.

## Fresh-Eyes Review

Re-read the new `#[error(...)]` attributes against the removed match arms. The
only meaningful risk was text drift from `thiserror` interpolation, so I added
exact display-string tests for both variants plus an explicit no-source check.
No bug was found after the reread.

## Verification

- `rustfmt --edition 2024 --check src/pages/size.rs`
- `git diff --check -- src/pages/size.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib pages::size::tests::test_size_error`

