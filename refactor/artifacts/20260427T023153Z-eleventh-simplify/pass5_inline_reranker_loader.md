# Pass 5 - Inline Reranker Loader

- Mission: Wrapper Collapse.
- Files changed: `src/search/reranker_registry.rs`.
- Change: inlined the private one-call `load_reranker_by_name(...)` helper into `get_reranker(...)`.
- Isomorphism proof: `get_reranker(...)` still validates or selects the same `RegisteredReranker` first, then dispatches on the same registered name strings and constructs the same unavailable errors via `rerank_failed(...)`.
- Fresh-eyes check: re-read both former match arms after inlining and confirmed the ONNX-backed reranker list, model-dir lookup, and not-implemented error text/source are unchanged.
- Verification:
  - `rustfmt --edition 2024 --check src/search/reranker_registry.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib search::reranker_registry::tests::`

Verdict: PRODUCTIVE
