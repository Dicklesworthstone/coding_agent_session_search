# Pass 2/10 - Local Type Alias Audit

## Isomorphism Card

### Change

Remove the private `SourcesRowEphemeralState` alias from `src/ui/app.rs` and expand its only typed use to `(bool, Option<(usize, usize, usize)>)`.

### Equivalence Contract

- Inputs covered: `CassApp::rebuild_sources_view` preservation of per-row busy state and doctor summary.
- Type behavior: unchanged. The alias was exactly `(bool, Option<(usize, usize, usize)>)`.
- Ownership behavior: unchanged. The map still stores copied `(busy, doctor_summary)` values and uses `.copied()` at lookup.
- Public API / schema: unchanged. The alias was private to `src/ui/app.rs`.
- Runtime behavior: unchanged. Type aliases do not generate runtime code.

### Candidate Score

- LOC saved: 1
- Confidence: 5
- Risk: 1
- Score: 5.0
- Decision: accept. The alias was used only to name a one-function ephemeral tuple and added an extra symbol without documenting a cross-function contract.

## Files Changed

- `src/ui/app.rs`: removed `SourcesRowEphemeralState` and expanded the map value type.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass2_sources_alias_audit.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read the alias definition and the single typed use.
- Confirmed the tuple element order stayed `(busy, doctor_summary)`.
- Confirmed `doctor_summary` is still `Option<(pass, warn, fail)>` and the map lookup still falls back to `(false, None)`.

## Verification

- Passed: `rustfmt --edition 2024 --check src/ui/app.rs`
- Passed: `git diff --check -- src/ui/app.rs refactor/artifacts/20260425T154730Z-third-simplify/pass2_sources_alias_audit.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib rebuild_sources_view` (4 passed)
