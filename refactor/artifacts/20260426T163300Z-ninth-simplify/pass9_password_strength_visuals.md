# Pass 9 - Enum/String Table: password strength visuals

## Change

Centralized `PasswordStrength` visual strings and percentages into one private `PasswordStrengthVisuals` match table. Existing public methods (`color`, `label`, `bar`, `percent`) now delegate to that table.

## Score

- LOC saved: 3
- Confidence: 5
- Risk: 1
- Score: 15.0
- Verdict: PRODUCTIVE

## Isomorphism Card

- Inputs covered: Weak, Fair, Good, Strong color, label, display, bar, and percent methods.
- Ordering preserved: N/A.
- Tie-breaking: N/A.
- Error semantics: unchanged; these methods are infallible.
- Laziness: unchanged.
- Short-circuit eval: unchanged.
- Floating-point: N/A.
- RNG/hash order: unchanged.
- Observable side effects: unchanged; no terminal rendering call sites were changed.
- Type narrowing: unchanged enum variants and public method signatures.

## Fresh-Eyes Review

Re-read the centralized visual table against the removed per-method matches. Confirmed each strength retains the same color, label, bar, and percent; `Display` still delegates to `label()`, and terminal rendering call sites still use the same public methods.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/password.rs`
- Passed: `git diff --check -- src/pages/password.rs .skill-loop-progress.md refactor/artifacts/20260426T163300Z-ninth-simplify/pass9_password_strength_visuals.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib pages::password::tests::test_strength_`
