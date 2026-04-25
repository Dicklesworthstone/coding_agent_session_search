# Pass 9/10 - Wrapper/Forwarder Census

## Isomorphism Card

### Change

Removed the private one-call `unique_staged_site_backup_dir(...)` wrapper and inlined its exact `unique_atomic_sidecar_path(final_dir, "bak", "site")` call at the only call site.

### Equivalence Contract

- Backup path shape: unchanged.
- Suffix: unchanged: `bak`.
- Fallback file name: unchanged: `site`.
- Sidecar uniqueness behavior: unchanged because `unique_atomic_sidecar_path(...)` is still used directly.
- Public API: unchanged; removed helper was private.

### Candidate Score

- LOC saved: 3
- Confidence: 5
- Risk: 1
- Score: 15.0
- Decision: accept. This is a direct single-call private wrapper collapse.

## Files Changed

- `src/pages/key_management.rs`: removed `unique_staged_site_backup_dir(...)` and inlined its body at the only call site.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass9_key_management_wrapper_collapse.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-ran a callsite census and confirmed no remaining references to `unique_staged_site_backup_dir`.
- Re-read the removed wrapper and replacement expression; suffix and fallback name are identical.
- Left the sibling sidecar helpers in place because they have separate call sites and clearer naming value.

## Verification

- Passed: `rustfmt --edition 2024 --check src/pages/key_management.rs`
- Passed: `git diff --check -- src/pages/key_management.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass9_key_management_wrapper_collapse.md .skill-loop-progress.md`
- Passed: `rg -n "unique_staged_site_backup_dir" src/pages/key_management.rs` returned no matches
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib pages::key_management::tests::test_replace_dir_from_temp_overwrites_existing_site`
