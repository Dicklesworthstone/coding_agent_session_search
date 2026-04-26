# Pass 8 - Embedder registry fixture helper

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Test Fixture Narrowing
- Scope: `src/search/embedder_registry.rs`
- Verdict: PRODUCTIVE

## Change

Added the test-local `registry_fixture()` helper and reused it across the
embedder registry tests that repeatedly created a temp directory and registry.

## Isomorphism Card

Preserved behavior:

- Every converted test still receives a fresh temporary data directory.
- The registry is still built with `EmbedderRegistry::new(tmp.path())`.
- The helper returns the `TempDir` with the registry so the path lifetime remains
  explicit.
- The missing-files test still uses `tmp.path()` directly for its model path
  assertion.

## Fresh-Eyes Review

Re-read the converted tests after the helper extraction. The only meaningful
risk was accidentally dropping the tempdir too early; returning `(TempDir,
EmbedderRegistry)` and binding `_tmp` in converted tests preserves the lifetime.

Yes, preservation was verified according to the skill: the whole
`search::embedder_registry::tests::` module passed after the fixture conversion.

## Verification

- `rustfmt --edition 2024 --check src/search/embedder_registry.rs`
- `git diff --check -- src/search/embedder_registry.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib search::embedder_registry::tests::`

