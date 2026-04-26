# Pass 6 - Error Mapping Helper

- Run: `20260425T234742Z-seventh-simplify`
- Timestamp: `2026-04-26T00:15:26Z`
- Mission: Error Mapping Helper
- Files changed: `src/search/fastembed_embedder.rs`

## Change

Added `FastEmbedder::unavailable_error(...)` and routed repeated `EmbedderError::EmbedderUnavailable` construction through it.

## Isomorphism Card

- Model-directory-missing errors still use the same model ID and `model directory not found: ...` reason.
- Missing ONNX errors still mention the same directory and checked filenames.
- Missing required model-file errors still list the same missing filenames.
- Unknown embedder and missing config errors still use the requested embedder name.
- Required-file read errors still preserve the same label, path, and I/O error text.
- `EmbedderUnavailable` still has no error source.

## Fresh-Eyes Review

Re-read each replacement against the removed struct literals. The helper only centralizes field assignment; every caller still supplies the original `model` and `reason` values.

## Verification

- `rustfmt --edition 2024 --check src/search/fastembed_embedder.rs`
- `git diff --check -- src/search/fastembed_embedder.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib search::fastembed_embedder::tests::`

## Verdict

PRODUCTIVE
