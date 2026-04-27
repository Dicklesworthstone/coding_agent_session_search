# Pass 2 - Semantic Availability TUI Matrix

- Mission: Model Availability Matrix
- Files changed: `src/search/model_manager.rs`
- Commit: pending

## Change

Converted the repeated simple TUI-state assertions in `test_semantic_availability_tui_states` into an explicit matrix of `(SemanticAvailability, status_label, predicate)`.

## Isomorphism Check

- Production code unchanged.
- The same five TUI states remain covered: `NotInstalled`, `NeedsConsent`, `Verifying`, `HashFallback`, and `Disabled`.
- Status labels are unchanged: `LEX`, `LEX`, `VFY...`, `SEM*`, and `OFF`.
- The same predicate checks remain enforced:
  - `is_not_installed()`
  - `needs_consent()`
  - summary contains `verifying`
  - `is_hash_fallback()` plus `can_search()`
  - `is_disabled()` plus summary contains `offline`

## Fresh-Eyes Review

Prompt applied: "Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with \"fresh eyes\" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?"

Yes. I re-read each matrix row against the removed assertions and verified the exact label and predicate coverage are preserved. No follow-up fix was needed.

## Verification

- `rustfmt --edition 2024 --check src/search/model_manager.rs`
- `git diff --check -- src/search/model_manager.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass2_semantic_availability_tui_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourteenth_simplify cargo test --lib search::model_manager::tests::test_semantic_availability_tui_states -- --exact`
- `ubs src/search/model_manager.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass2_semantic_availability_tui_matrix.md` reported no critical issues.
