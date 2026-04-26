# Pass 6 - Inline Track A Rebuild Check

## Mission

Inline one private one-call wrapper with no independent contract.

## Change

Removed `analytics_track_a_rebuild_safe(...)` and inlined its three-table existence check at the only callsite in the Track A analytics repair branch.

## Isomorphism Card

- Inputs covered: Track A analytics repair decisions.
- Ordering preserved: table checks still evaluate `messages`, `conversations`, then `agents`.
- Tie-breaking: unchanged; all three tables must exist.
- Error semantics: unchanged; `table_exists` still returns the same booleans and the same repair branch is skipped when any table is missing.
- Laziness: unchanged; `.all(...)` still short-circuits on first missing table.
- Short-circuit eval: preserved.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side effects: unchanged; table-existence checks are read-only.
- Robot JSON / public contracts: unchanged.

## Fresh-Eyes Review

Confirmed the removed helper had exactly one callsite and contained only:

```rust
["messages", "conversations", "agents"]
    .into_iter()
    .all(|table| analytics::query::table_exists(conn, table))
```

The inlined expression uses the same table list, same order, same `table_exists` function, and the same `pre_conn` connection.

## Verification

- `rustfmt --edition 2024 --check src/lib.rs`
- `git diff --check -- src/lib.rs .skill-loop-progress.md refactor/artifacts/20260426T210630Z-tenth-simplify/pass6_inline_track_a_rebuild_check.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_tenth_simplify cargo check --lib`

## LOC Delta

- `src/lib.rs`: 4 insertions, 7 deletions.
- Net: -3 lines.

## Verdict

PRODUCTIVE. The pass removed a private one-call wrapper while preserving the exact read-only safety check.
