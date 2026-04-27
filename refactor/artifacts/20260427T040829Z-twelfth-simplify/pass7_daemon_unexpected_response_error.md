# Pass 7 - Daemon Unexpected Response Error Helper

## Target

- File: `src/daemon/client.rs`
- Seam: repeated daemon unexpected-response errors

## Simplification

Centralized repeated `DaemonError::Failed(format!("unexpected response: {:?}", other))` construction in `unexpected_response(...)`.

## Isomorphism Card

- The rendered error still starts with `daemon failed: unexpected response:`.
- The response payload is still formatted with Rust `Debug`.
- Every converted branch still returns `Err(...)` for the non-matching response variant.
- Success arms for health, shutdown, jobs, embedding, batch embedding, and reranking are untouched.

## Fresh-Eyes Review

Re-read every converted match arm and confirmed each previous `other` binding now flows into the helper without changing ownership, branch order, or success behavior. Added an exact text test for the centralized constructor.

## Verification

- `rustfmt --edition 2024 --check src/daemon/client.rs`
- `git diff --check -- src/daemon/client.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib daemon::client::tests::unexpected_response_error_text_is_stable`

## Verdict

PRODUCTIVE. Repeated local error construction is now one helper with exact text coverage.
