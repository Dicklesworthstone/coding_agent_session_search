# Pass 8 - Bookmark Search Assertion Helper

## Change
- Extracted the test-local `assert_single_search_path(...)` helper in `src/bookmarks.rs`.
- Replaced the repeated percent, underscore, and backslash search assertion clusters with helper calls.
- The helper compares the complete matched source-path vector to a single expected path, so count, ordering, and path content remain covered.

## Fresh-Eyes Review
- Re-read all three replaced cases and confirmed the exact query/path pairs are still asserted:
  - `%` -> `/percent.rs`
  - `_` -> `/underscore.rs`
  - `\\` -> `/backslash.rs`
- Confirmed the helper still unwraps `store.search(query)` like the original test and only changes assertion shape.
- Confirmed no production bookmark behavior changed.

## Verification
- `rustfmt --edition 2024 --check src/bookmarks.rs`
- `git diff --check -- src/bookmarks.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib bookmarks::tests::test_search_treats_like_metacharacters_literally`

## Verdict
PRODUCTIVE: reduced repeated test assertions while preserving all three literal search-contract checks.
