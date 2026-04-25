# Pass 1 - PreviewError Derive

- Mission: Derive/Boilerplate Audit
- Score: 3.0
- Files changed: `src/pages/preview.rs`

## Change

`PreviewError` no longer carries hand-written `Display` and `Error` implementations. Each variant now declares its exact display string with `thiserror::Error`, and the existing `source` fields continue to provide error sources through the derive.

## Isomorphism Proof

- `BindFailed { port, source }` still displays `Failed to bind to port {port}: {source}` and exposes `source`.
- `SiteDirectoryNotFound(path)` still displays `Site directory not found: {path.display()}` and has no source.
- `FileReadError { path, source }` still displays `Failed to read file {path.display()}: {source}` and exposes `source`.
- `BrowserOpenFailed(msg)` still displays `Failed to open browser: {msg}` and has no source.
- `ServerError(msg)` still displays `Server error: {msg}` and has no source.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read the derive attributes and the new parity test against the removed manual implementations. No bugs found. The focused test verifies all display strings plus both source-preserving variants and all no-source variants.

## Verification

- `rustfmt --edition 2024 --check src/pages/preview.rs`
- `git diff --check -- src/pages/preview.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib pages::preview::tests::test_preview_error_display_and_source_are_preserved`
