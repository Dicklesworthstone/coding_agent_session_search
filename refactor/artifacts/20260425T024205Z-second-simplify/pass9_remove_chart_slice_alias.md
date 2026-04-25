# Pass 9/10 - Type Alias/Local Tuple Audit

## Isomorphism Card

### Change

Remove the private `StrF64Slice<'a>` alias from `src/ui/analytics_charts.rs` and expand its two uses to `&[(String, f64)]`.

### Equivalence Contract

- Inputs covered: explorer overlay dimension breakdown selection and `build_dimension_overlay`.
- Type behavior: unchanged. `StrF64Slice<'_>` was exactly `&[(String, f64)]`.
- Lifetime behavior: unchanged. The elided slice lifetime in `&[(String, f64)]` has the same callsite inference as the alias use.
- Runtime behavior: unchanged. Type aliases do not generate runtime code.
- Public API / schema: unchanged. The alias was private and local to the module.

### Candidate Score

- LOC saved: 1
- Confidence: 5
- Risk: 1
- Score: 5.0
- Decision: accept. The alias hid a simple slice type and did not remove enough complexity to justify itself.

## Files Changed

- `src/ui/analytics_charts.rs`: removed `StrF64Slice` and expanded two uses.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass9_remove_chart_slice_alias.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read both replacement sites and the removed alias.
- Confirmed no public type name or exported API referenced `StrF64Slice`.
- Confirmed `build_dimension_overlay` still receives a borrowed slice and does not take ownership of chart vectors.
- Confirmed overlay selection still returns borrowed slices from `AnalyticsChartData`.

## Verification

- Passed: `rustfmt --edition 2024 --check src/ui/analytics_charts.rs`
- Passed: `git diff --check -- src/ui/analytics_charts.rs refactor/artifacts/20260425T024205Z-second-simplify/pass9_remove_chart_slice_alias.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --lib ui::analytics_charts::` (32 passed)
