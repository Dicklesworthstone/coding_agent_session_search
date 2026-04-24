# Isomorphism Card - HTML Export Error Derives

## Change
Replace three hand-written `Display` + empty `std::error::Error` implementations with `thiserror::Error` derives in `src/html_export/encryption.rs`, `src/html_export/template.rs`, and `src/html_export/renderer.rs`.

## Opportunity Matrix
| Candidate | LOC | Confidence | Risk | Score | Decision |
|-----------|-----|------------|------|-------|----------|
| D1: derive Error for three HTML export error enums | 3 | 5 | 1 | 15.0 | Accept |

## Equivalence Contract
- Inputs covered: all existing construction and formatting of `EncryptionError`, `TemplateError`, and `RenderError`.
- Ordering preserved: N/A; no iteration or side effects.
- Tie-breaking: N/A.
- Error semantics: same enum variants; same `Display` strings; still implements `std::error::Error`; no source chain added.
- Laziness: N/A.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: unchanged; no logs, metrics, files, DB writes, or HTML output paths are touched.
- Type narrowing: Rust enum pattern matches and type names remain unchanged.
- Rerender behavior: N/A.

## Verification Plan
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify cargo check --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify cargo test html_export --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify cargo clippy --all-targets -- -D warnings`
- `rustfmt --edition 2024 --check src/html_export/encryption.rs src/html_export/template.rs src/html_export/renderer.rs`
- `ubs src/html_export/encryption.rs src/html_export/template.rs src/html_export/renderer.rs`
