# Pass 4 - Preview Site Test Fixture

- Mission: Test Fixture Helper
- Score: 2.0
- Files changed: `src/pages/preview.rs`

## Change

Preview tests that need a temporary site with an `index.html` now use test-local helper `temp_site_with_index(...)`.

## Isomorphism Proof

- Every converted test still owns the returned `TempDir` for the full assertion scope.
- String fixtures and the large byte-vector fixture are written to the same `index.html` path.
- The service-worker fixture remains test-specific because only that test needs `sw.js`.
- Existing preview tests continue to assert the same response status lines, content types, bodies, and content lengths.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read the helper and all converted callsites. No bugs found. The helper accepts bytes so the large-file HEAD test still writes the same payload, and each test retains the temp directory until after request handling.

## Verification

- `rustfmt --edition 2024 --check src/pages/preview.rs`
- `git diff --check -- src/pages/preview.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib pages::preview::tests::`
