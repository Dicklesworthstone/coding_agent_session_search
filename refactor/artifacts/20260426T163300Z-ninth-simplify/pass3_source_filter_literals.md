# Pass 3 - Literal Consolidation: SourceFilter spellings

## Change

Pinned the three built-in `SourceFilter` spellings (`all`, `local`, `remote`) as private constants in `src/sources/provenance.rs` and reused them in parsing, display, and focused tests.

## Score

- LOC saved: 1
- Confidence: 5
- Risk: 1
- Score: 5.0
- Verdict: PRODUCTIVE

## Isomorphism Card

- Inputs covered: `SourceFilter::parse`, `SourceFilter::Display`, uppercase normalization, wildcard all, whitespace trimming, source-id fallback.
- Ordering preserved: N/A; no iteration changed.
- Tie-breaking: N/A.
- Error semantics: N/A; parsing still returns `SourceId(trimmed)` for unrecognized non-empty inputs.
- Laziness: unchanged.
- Short-circuit eval: unchanged match arm order; blank, `all`, wildcard still map to `All` before built-ins and fallback.
- Floating-point: N/A.
- RNG/hash order: N/A.
- Observable side effects: unchanged; no logs, metrics, I/O, or DB writes.
- Type narrowing: unchanged enum variants and public serialization.

## Fresh-Eyes Review

Re-read the parse match, display match, and tests after editing. The constants are private, the wildcard remains a literal because it is an alias rather than a displayed spelling, uppercase literal tests still prove normalization, and `SourceId` display/parsing still preserves caller-provided IDs after trimming only at parse time.

## Verification

- Passed: `rustfmt --edition 2024 --check src/sources/provenance.rs`
- Passed: `git diff --check -- src/sources/provenance.rs .skill-loop-progress.md refactor/artifacts/20260426T163300Z-ninth-simplify/pass3_source_filter_literals.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib sources::provenance::tests::test_source_filter`
