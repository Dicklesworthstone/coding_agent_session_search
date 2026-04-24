# Simplification Dashboard - 20260424T222109Z-codex-simplify

## Scope
- Docs read: `AGENTS.md` (1166 lines), `README.md` (2867 lines), skill instructions.
- Architecture mapped: CLI entry (`src/main.rs`) -> clap command orchestration (`src/lib.rs`) -> connector/FAD ingestion -> frankensqlite canonical storage -> frankensearch/Tantivy/vector derived assets -> TUI and robot JSON surfaces.
- Accepted candidate: D1, HTML export error trait boilerplate in `src/html_export/encryption.rs`, `src/html_export/template.rs`, and `src/html_export/renderer.rs`.

## Metrics
| Metric | Before | After | Delta | Status |
|--------|--------|-------|-------|--------|
| Production code LOC | 3 files | 3 files | -33 net lines | pass |
| Total touched LOC | 3 files | 3 files | +11 net lines after regression tests | pass |
| Candidate score | N/A | 15.0 | accepted | pass |
| Duplication scanner | no external dup tools installed | manual D1 recorded | artifact present | pass |
| AI-slop scan | completed | no direct D1 blocker | artifact present | pass |
| Golden/API schema | unchanged | unchanged | no golden regen needed | pass |
| Touched-file formatting | clean | clean | 0 warnings | pass |
| Full `cargo check --all-targets` | clean | clean | 0 errors | pass |
| Focused tests | N/A | 138 passed, 0 failed | pass | pass |
| Full clippy | blocked by unrelated current-tree lint | blocked | blocker recorded | blocked |

## Verification
- PASS: `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify cargo check --all-targets`
- PASS: `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify cargo test html_export --lib`
- PASS: `rustfmt --edition 2024 --check src/html_export/encryption.rs src/html_export/template.rs src/html_export/renderer.rs`
- PASS: `git diff --check -- src/html_export/encryption.rs src/html_export/template.rs src/html_export/renderer.rs refactor/artifacts/20260424T222109Z-codex-simplify`
- PASS with pre-existing UBS findings: `ubs src/html_export/encryption.rs src/html_export/template.rs src/html_export/renderer.rs`
- BLOCKED: `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify cargo clippy --all-targets -- -D warnings`
  - Current exact run reports `src/search/query.rs:5859`, `clippy::assertions-on-constants`, outside the touched HTML export files.
  - Earlier exact run reported `src/lib.rs:5616-5620`, `clippy::doc-overindented-list-items`, while that file was exclusively reserved by `SwiftBison`.
  - Diagnostic rerun allowing only those unrelated blockers passed: `cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants -A clippy::doc-overindented-list-items`.

## Ledger
| ID | Change | Files | Net LOC | Proof |
|----|--------|-------|---------|-------|
| D1 | Replace manual `Display` + empty `Error` impls with `thiserror::Error` derives | `src/html_export/encryption.rs`, `src/html_export/template.rs`, `src/html_export/renderer.rs` | -33 production, +11 total with tests | isomorphism card + check + focused tests |
| F1 | Add executable regression tests for preserved error display strings | same files | +44 test lines | `cargo test html_export --lib` |

## Rejection Log
| Candidate | Decision | Reason |
|-----------|----------|--------|
| P16 broader `*Error` enum sweep | defer | Several remaining errors preserve custom `source()` behavior or path formatting; outside this proof's narrow HTML export scope. |
| P22 string status comparisons | reject for this pass | Requires semantic/domain audit and likely constants/types; not a mechanical LOC-negative proof. |
| Connector stub file deletion | reject | File deletion is forbidden without explicit written permission, and public module paths may still be import contracts. |
