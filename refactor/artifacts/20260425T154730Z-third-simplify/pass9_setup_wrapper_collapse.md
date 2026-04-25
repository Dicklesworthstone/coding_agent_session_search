# Pass 9/10 - Wrapper Hop Collapse

## Isomorphism Card

### Change

Remove private single-call wrapper `generated_source_name_key_for_host(...)` from `src/sources/setup.rs` and inline its exact body at the only callsite.

### Equivalence Contract

- Normalization: unchanged. The callsite still runs `generated_source_name_for_host(...)` before `source_name_key(...)`.
- Duplicate detection: unchanged. The same generated source-name key is inserted into `selected_name_keys`.
- Scope: unchanged. `generated_source_name_for_host(...)` remains private and is still shared by preview and dedupe logic.
- Public behavior: unchanged. Non-interactive source setup should select the same hosts and skip the same generated-name duplicates.

### Candidate Score

- Wrapper hops removed: 1
- Confidence: 5
- Risk: 1
- Score: 5.0
- Decision: accept. This removes a private one-call forwarding helper with no independent contract.

## Files Changed

- `src/sources/setup.rs`: inlined and removed the private wrapper.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass9_setup_wrapper_collapse.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read the removed function and the replacement expression; they are the same `source_name_key(generated_source_name_for_host(host_name))` composition.
- Confirmed the generated source name is still borrowed only for the duration of `source_name_key`, matching the removed wrapper.
- Confirmed no other callsites existed before removal.

## Verification

- Passed: `rustfmt --edition 2024 --check src/sources/setup.rs`
- Passed: `git diff --check -- src/sources/setup.rs refactor/artifacts/20260425T154730Z-third-simplify/pass9_setup_wrapper_collapse.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib sources::setup::` (19 passed)
