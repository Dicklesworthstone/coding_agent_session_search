# Pass 2 - Progress Snapshot Helper

## Mission

Name one repeated snapshot/progress projection in the indexer without changing robot JSON fields.

## Change

Replaced four repeated JSON string/null branches in `IndexingProgress::snapshot_json(...)` with two local helpers:

- `non_empty_json_string(...)`
- `active_rebuild_json_string(...)`

The helper return type is `Option<String>` because `serde_json::json!` serializes `None` to `null` and `Some(value)` to the same string value as the removed explicit branches.

## Isomorphism Card

- Inputs covered: progress snapshots with active rebuild telemetry and non-empty controller reason strings.
- Ordering preserved: JSON object keys and snapshot field order are unchanged at the call sites.
- Tie-breaking: N/A.
- Error semantics: unchanged; lock fallback behavior and string cloning before projection are unchanged.
- Laziness: unchanged; strings are loaded before JSON projection exactly as before.
- Short-circuit eval: preserved; staged reason strings still require `is_rebuilding && !value.is_empty()`.
- Floating-point: unchanged; load-average conversion remains untouched.
- RNG / hash order: N/A.
- Observable side effects: unchanged; snapshot projection has no external side effects.
- Robot JSON / public contracts: field names and values are unchanged.

## Fresh-Eyes Review

Re-read the removed branches against the helpers:

- Non-empty controller mode/reason still serializes as JSON strings.
- Empty controller mode/reason still serializes as JSON `null`.
- Staged merge and staged shard-build reasons still serialize to `null` when not rebuilding, even if the stored reason string is non-empty.
- Staged reason strings still serialize to `null` while rebuilding if the reason is empty.
- No schema fields were added, removed, or renamed.

## Verification

- `rustfmt --edition 2024 --check src/indexer/mod.rs`
- `git diff --check -- src/indexer/mod.rs .skill-loop-progress.md refactor/artifacts/20260426T210630Z-tenth-simplify/pass2_progress_snapshot_helper.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_tenth_simplify cargo test --lib snapshot_json_`

## LOC Delta

- `src/indexer/mod.rs`: 12 insertions, 20 deletions.
- Net: -8 lines.

## Verdict

PRODUCTIVE. The pass removed repeated JSON projection branches while preserving the snapshot contract.
