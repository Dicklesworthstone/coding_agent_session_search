# Pass 8 - Daemon Status Projection Helper

## Target

- File: `src/daemon/core.rs`
- Seam: `Request::Status` response assembly

## Simplification

Extracted private `embedder_model_info(...)`, `reranker_model_info(...)`, and `status_response(...)` methods so `Request::Status` delegates to a named status projection.

## Isomorphism Card

- `uptime_secs`, `version`, `memory_bytes`, and `total_requests` are populated from the same sources.
- Embedder fields still project `id`, `name`, `dimension`, `loaded`, and `memory_bytes` in the same way.
- Reranker fields still project `id`, `name`, `dimension: None`, `loaded`, and `memory_bytes` in the same way.
- `Request::Status` still returns `Response::Status(...)`; no protocol field names or types changed.

## Fresh-Eyes Review

Re-read the old inline projection against the new helper methods and verified each field source moved one-for-one. The new test pins the default unloaded model projection.

## Verification

- `rustfmt --edition 2024 --check src/daemon/core.rs`
- `git diff --check -- src/daemon/core.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib daemon::core::tests::status_response_projects_default_model_fields`

## Verdict

PRODUCTIVE. Status response assembly is now easier to audit and exact default field parity is tested.
