# Pass 10/10 - Final Rescan and Dashboard

## Isomorphism Card

### Change

Final rescan found repeated positive generated-document content assertions in `src/pages/docs.rs`; add a test-only `assert_doc_contains!` macro and use it for those fixed-string checks.

### Equivalence Contract

- Assertion predicate: unchanged. Every converted check still evaluates `doc.content.contains("<literal>")`.
- Failure expression: preserved through macro expansion rather than moving to a helper function with opaque variables.
- Negative assertions and placeholder diagnostics: unchanged, because their custom predicates/messages differ.
- Runtime behavior: unchanged. Only `#[cfg(test)]` code changed.
- Public docs output: unchanged. Generator implementation and templates were not modified in this pass.

### Candidate Score

- Repeated assertion sites collapsed: 30
- Confidence: 5
- Risk: 1
- Score: 135.0
- Decision: accept. This is a test-only assertion simplification discovered during final rescan of touched docs code.

## Files Changed

- `src/pages/docs.rs`: added a test-only generated-doc containment assertion macro and converted repeated positive assertions.
- `src/ui/app.rs`: fixed the final clippy finding from pass 2 by replacing the exposed complex tuple map value with private `SourcesRowEphemeralState`.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass10_docs_assertion_macro.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read every converted generated-document assertion and confirmed each checked literal remains identical.
- Corrected this proof card's converted-assertion count from 27 to 30 after the reread.
- Confirmed the negative `Good news!` assertion and placeholder assertions remain explicit because their predicates or diagnostics differ.
- Re-read the `SourcesRowEphemeralState` clippy fix and confirmed it preserves `busy=false` and `doctor_summary=None` defaults while giving the row cache a named private shape.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/docs.rs`
- Passed: `rustfmt --edition 2024 --check src/ui/app.rs src/pages/docs.rs`
- Passed: `git diff --check -- src/pages/docs.rs refactor/artifacts/20260425T154730Z-third-simplify/pass10_docs_assertion_macro.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib pages::docs::` (11 passed)
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib rebuild_sources_view` (4 passed)
- Passed: touched-file rustfmt for all third-loop code files.
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo check --all-targets`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo clippy --all-targets -- -D warnings`
- Known unrelated blocker: full `cargo fmt --check` still reports pre-existing formatting drift in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, and `tests/metamorphic_agent_detection.rs`.
