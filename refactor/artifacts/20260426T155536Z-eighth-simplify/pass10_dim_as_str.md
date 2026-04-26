# Pass 10 - Dim string helper

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Final Rescan and Dashboard
- Scope: `src/analytics/types.rs`
- Verdict: PRODUCTIVE

## Change

Added `Dim::as_str()` as the single lowercase spelling source for analytics
dimensions, then made `Display` delegate to it.

## Isomorphism Card

Preserved behavior:

- `Agent`, `Workspace`, `Source`, and `Model` still display as `agent`,
  `workspace`, `source`, and `model`.
- Serde remains governed by `#[serde(rename_all = "lowercase")]`.
- Query surfaces that use `self.dim.to_string()` still receive the same strings.
- No metric, grouping, rollup, or query logic changed.

## Fresh-Eyes Review

Re-read the `Dim` implementation and nearby `Metric::as_str()` pattern. The
new helper mirrors the established metric pattern and leaves the enum variants
and serde annotations unchanged.

Yes, preservation was verified according to the skill: the focused test asserts
both `as_str()` and `to_string()` for every variant.

## Verification

- `rustfmt --edition 2024 --check src/analytics/types.rs`
- `git diff --check -- src/analytics/types.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib analytics::types::tests::dim_as_str_matches_display_for_all_variants`

