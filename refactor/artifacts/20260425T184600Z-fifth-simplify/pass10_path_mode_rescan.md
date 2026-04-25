# Pass 10 - Path Mode Rescan

## Change
- Re-scanned the fifth-loop changed surfaces and found one remaining duplicate normalization path in `PagesConfig::path_mode()`.
- Reused `normalized_path_mode()` in `path_mode()` instead of repeating trim/lowercase handling.
- Added an explicit blank-path-mode assertion to pin the old fallback behavior as `PathMode::Relative`.

## Fresh-Eyes Review
- Re-read the shared normalizer and confirmed it preserves the prior `path_mode()` behavior:
  - `None` still resolves to `Relative`.
  - `basename`, `full`, and `hash` still map to their corresponding enum variants.
  - case and surrounding whitespace remain tolerated.
  - blank strings still fall through to `Relative`.
- Confirmed this only unifies parsing logic and does not alter validation error text or resolved config defaults.

## Verification
- `rustfmt --edition 2024 --check src/pages/config_input.rs`
- `git diff --check -- src/pages/config_input.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib pages::config_input::tests::test_path_mode_parsing`

## Verdict
PRODUCTIVE: removed duplicated path-mode normalization after the final rescan and pinned the blank-input fallback.
