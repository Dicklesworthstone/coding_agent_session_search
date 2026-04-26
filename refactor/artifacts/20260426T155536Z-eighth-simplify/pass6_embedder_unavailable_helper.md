# Pass 6 - EmbedderUnavailable constructor helper

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Local Error Constructor
- Scope: `src/search/embedder_registry.rs`
- Verdict: PRODUCTIVE, with unrelated cargo-test blocker

## Change

Added the private `embedder_unavailable(model, reason)` helper and routed the
registry's repeated `EmbedderError::EmbedderUnavailable` construction through it.

## Isomorphism Card

Preserved behavior:

- Unknown embedder validation still reports the requested model name and the
  same available-embedder list.
- Missing model-file validation still reports the same model name, model
  directory, missing file list, and install hint.
- Unknown implementation fallback still reports `embedder not implemented`.
- Registry lookup order, availability checks, model directory lookup, and
  embedder loading are unchanged.

## Fresh-Eyes Review

Re-read each replacement against the removed struct literals. The helper only
normalizes construction of the error variant; every caller still owns its
original reason string.

Yes, preservation was verified according to the skill by checking every caller
and adding a helper-shape test for the exact `model` and `reason` fields. The
focused cargo test was attempted but blocked by unrelated dirty work in
`src/storage/sqlite.rs`.

## Verification

Passed:

- `rustfmt --edition 2024 --check src/search/embedder_registry.rs`
- `git diff --check -- src/search/embedder_registry.rs`

Blocked:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib search::embedder_registry::tests::test_embedder_unavailable_helper_shape`
- Blocker: pre-existing dirty `src/storage/sqlite.rs` fails to compile at line
  4241 with `error: expected token: ','`.

