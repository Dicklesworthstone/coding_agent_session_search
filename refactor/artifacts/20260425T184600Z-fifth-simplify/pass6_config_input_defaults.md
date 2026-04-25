# Pass 6 - Config Input Defaults

## Change
- Extracted the private resolved-config defaults for path mode, compression, and chunk size.
- Routed `PagesConfig::to_resolved()` through local helpers instead of repeating inline fallback literals.
- Added a focused test pinning the resolved default values produced by `validate()`.

## Fresh-Eyes Review
- Re-read the old inline fallbacks against the new constants:
  - `relative` remains the default resolved path mode.
  - `deflate` remains the default compression.
  - `8 * 1024 * 1024` remains the default chunk size.
- Confirmed `normalized_path_mode()` still trims, rejects empty strings, and lowercases specified path modes before applying the default.
- Confirmed no validation priority changed: invalid explicit values still error before `to_resolved()` is used.

## Verification
- `rustfmt --edition 2024 --check src/pages/config_input.rs`
- `git diff --check -- src/pages/config_input.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib pages::config_input::tests::test_resolved_config_applies_export_defaults`

## Verdict
PRODUCTIVE: reduced default-literal drift in resolved export configuration with an isolated behavior pin.
