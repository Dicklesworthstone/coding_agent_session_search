# Pass 9 - GroupBy string census

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Enum/String Census
- Scope: `src/analytics/types.rs`
- Verdict: PRODUCTIVE

## Change

Added `GroupBy::as_str()` as the single lowercase spelling source for
`GroupBy`, then made `Display` delegate to it.

## Isomorphism Card

Preserved behavior:

- `Hour`, `Day`, `Week`, and `Month` still display as `hour`, `day`, `week`,
  and `month`.
- `label()`, `next()`, and `prev()` are unchanged.
- Serde remains governed by `#[serde(rename_all = "lowercase")]`.
- Existing `GROUP_BY_CASES` still drives display, label, next, and previous
  coverage.

## Fresh-Eyes Review

Re-read the enum implementation and tests after the extraction. The display
match moved into `as_str()`, while human labels and cycle behavior remain in
their original methods.

Yes, preservation was verified according to the skill: the focused display test
now asserts both `as_str()` and `to_string()` against the existing case table.

## Verification

- `rustfmt --edition 2024 --check src/analytics/types.rs`
- `git diff --check -- src/analytics/types.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib analytics::types::tests::group_by_display`

