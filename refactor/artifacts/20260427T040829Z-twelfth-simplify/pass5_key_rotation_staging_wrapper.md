# Pass 5 - Key Rotation Staging Wrapper Collapse

## Target

- File: `src/pages/key_management.rs`
- Seam: `unique_staged_site_dir`

## Simplification

Removed the private one-call `unique_staged_site_dir(...)` wrapper and called `unique_atomic_sidecar_path(&archive_dir, "rotate", "site")` directly from key rotation.

## Isomorphism Card

- Staged key-rotation directory still uses suffix `rotate`.
- Fallback filename remains `site`.
- The generic sidecar path helper is unchanged.
- No file deletion command was used; this is only a Rust helper removal inside the existing file.

## Fresh-Eyes Review

Re-read the old wrapper and the new callsite. The inlined arguments exactly match the removed wrapper body, and the rest of the key-rotation staging, encryption, manifest, sync, and swap flow is untouched.

## Verification

- `rustfmt --edition 2024 --check src/pages/key_management.rs`
- `git diff --check -- src/pages/key_management.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib pages::key_management::tests::test_key_rotate -- --exact`

## Verdict

PRODUCTIVE. Removed a private one-call wrapper with exact argument parity and verified the real rotation path.
