# Pass 6 - Semantic Doc Component ID Conversion Helper

## Target

- File: `src/search/query.rs`
- Seam: progressive semantic doc-id hydration

## Simplification

Extracted `semantic_doc_component_id_from_db(...)` for repeated nullable signed-id to `u32` conversion used by progressive semantic `agent_id` and `workspace_id` fields.

## Isomorphism Card

- `None` legacy ids still become `0`.
- Negative legacy sentinel ids still become `0`.
- In-range positive ids still pass through unchanged.
- Values larger than `u32::MAX` still saturate to `u32::MAX`.
- Both exact-match and fallback progressive hydration queries now use the same conversion helper.

## Fresh-Eyes Review

Re-read both converted query row-mapping blocks against the removed match/if branches. The helper preserves the old branch outcomes for agent and workspace ids, and the new boundary test covers the edge cases that made the inline code easy to get wrong.

## Verification

- `rustfmt --edition 2024 --check src/search/query.rs`
- `git diff --check -- src/search/query.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib search::query::tests::semantic_doc_component_id_from_db_clamps_bounds`

## Verdict

PRODUCTIVE. Four repeated conversion branches are now one pure helper with explicit boundary coverage.
