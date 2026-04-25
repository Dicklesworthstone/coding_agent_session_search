# Pass 8/10 - Test Assertion DRY Pass

## Isomorphism Card

### Change

Add a test-only `assert_inline_js_contains!` macro in `src/html_export/scripts.rs` and use it for repeated positive `bundle.inline_js.contains("...")` assertions.

### Equivalence Contract

- Assertion predicate: unchanged. Every converted assertion still checks `bundle.inline_js.contains("<literal>")`.
- Failure expression: preserved through macro expansion rather than moving to a helper function with opaque variables.
- Negative assertion: unchanged. The one `!bundle.inline_js.contains("const Search")` check remains explicit.
- Runtime behavior: unchanged. Only `#[cfg(test)]` code changed.
- Coverage: unchanged. Every literal checked before this pass is still checked after this pass.

### Candidate Score

- Repeated assertion sites collapsed: 48
- Confidence: 5
- Risk: 1
- Score: 240.0
- Decision: accept. This is test-only assertion plumbing with no production impact.

## Files Changed

- `src/html_export/scripts.rs`: added the assertion macro and replaced repeated positive inline-JS containment assertions.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass8_inline_js_assertion_macro.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read every converted assertion and fixed one transcription issue where `"case '?':"` had briefly lost its trailing colon during the macro conversion.
- Confirmed each checked literal is still present after that fix.
- Confirmed the macro accepts literals only, matching this test module's fixed-string assertions.
- Confirmed the negative `const Search` exclusion assertion stayed outside the macro because its predicate is inverted.

## Verification

- Passed: `rustfmt --edition 2024 --check src/html_export/scripts.rs`
- Passed: `git diff --check -- src/html_export/scripts.rs refactor/artifacts/20260425T154730Z-third-simplify/pass8_inline_js_assertion_macro.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib html_export::scripts::tests::` (12 passed)
