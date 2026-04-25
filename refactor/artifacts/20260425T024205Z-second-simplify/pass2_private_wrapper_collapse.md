# Pass 2 - Private Wrapper Hop Collapse

## Change: Inline the Windows backup sidecar helper

### Candidate
- Target: `src/ui/app.rs`
- Wrapper: `unique_replace_backup_path(path)` -> `unique_atomic_sidecar_path(path, "bak", "tui_state.json")`
- Callsite count: 1, inside the `#[cfg(windows)]` branch of `replace_file_from_temp`
- Decision: accept. This is a private, single-use, constant-argument alias over the existing sidecar path constructor.

### Equivalence Contract
- Inputs covered: the single `final_path` callsite in the Windows replacement branch.
- Ordering preserved: yes. The `let backup_path = ...` statement remains in the same position before renaming the final file.
- Tie-breaking: unchanged. `unique_atomic_sidecar_path` still owns timestamp and nonce generation.
- Error semantics: unchanged. The same `PathBuf` value is produced before any filesystem operation; rename and formatting branches are untouched.
- Laziness: N/A. Both forms compute the backup path eagerly.
- Short-circuit eval: unchanged. No boolean/control-flow change.
- Floating-point: N/A.
- RNG / hash order: N/A. The atomic nonce increment in `unique_atomic_sidecar_path` is still executed once at the same point.
- Observable side-effects: identical. The only side effect is the helper's atomic nonce increment; logs, filesystem operations, and error text are unchanged.
- Type narrowing: N/A.
- Rerender behavior: N/A.

### Opportunity Matrix
- LOC saved: 1 (<5 lines)
- Confidence: 5 (single callsite, private helper, direct literal inlining)
- Risk: 1 (single file, private helper, no public API or robot schema)
- Score: 5.0

### Pre-edit Evidence
- `rg -n "unique_atomic_temp_path|unique_replace_backup_path|unique_atomic_sidecar_path|write_atomic" src/ui/app.rs tests --glob '!tests/metamorphic_agent_detection.rs' --glob '!tests/golden_robot_json.rs'`
- `wc -l src/ui/app.rs` -> `46102 src/ui/app.rs`
- `git diff -- src/ui/app.rs` -> empty before this pass

### Rejected Candidates
- `src/indexer/semantic.rs::semantic_backfill_scheduler_decision`: rejected because its live callsite is in `src/lib.rs`, which the mission marks high-risk unless the target is extremely tight and private-only. It also injects live capacity rather than being a pure same-argument alias.
- `src/ui/app.rs::unique_atomic_temp_path`: rejected because it has multiple callsites and inlining would spread the `"tmp"` and `"tui_state.json"` constants, increasing local noise for little gain.

### Verification
- `rustfmt --edition 2024 --check src/ui/app.rs` - passed.
- `git diff --check -- src/ui/app.rs refactor/artifacts/20260425T024205Z-second-simplify/pass2_private_wrapper_collapse.md .skill-loop-progress.md` - passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --lib persisted_state_temp_paths_are_unique` - passed: 1 test passed, 0 failed, 4122 filtered out.
- Targeted callsite census: `rg -n "unique_atomic_sidecar_path|unique_atomic_temp_path|replace_file_from_temp|tui_state|atomic" src/ui/app.rs tests -g '*.rs'`.
- The changed line is in a `#[cfg(windows)]` branch. Linux test execution does not exercise that branch, so the proof rests on direct expression equivalence: the removed private helper body and the replacement call are byte-for-byte the same function call with the same arguments.

### Fresh-Eyes Review
- The initial worker diff also removed embedded source registration in `src/indexer/mod.rs` and formatted unrelated tests. That was off-mission and high risk, so those changes were manually backed out before this pass was accepted.
- Re-read the remaining `src/ui/app.rs` diff after the cleanup. The only retained behavior change candidate is the private one-call helper inline described above.
- The atomic nonce side effect remains exactly once at the same execution point, inside `unique_atomic_sidecar_path`.
