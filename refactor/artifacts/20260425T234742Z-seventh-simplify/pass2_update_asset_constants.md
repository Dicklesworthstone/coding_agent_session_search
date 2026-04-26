# Pass 2 - Literal/Constant Table Tightening

- Run: `20260425T234742Z-seventh-simplify`
- Timestamp: `2026-04-26T00:02:13Z`
- Mission: Literal/Constant Table Tightening
- Files changed: `src/update_check.rs`

## Change

Added private release asset constants for the Unix installer, Windows installer, and checksums asset, then reused them in self-update URL construction and the associated tests.

## Isomorphism Card

- Unix self-update still downloads `install.sh`.
- Windows self-update still downloads `install.ps1`.
- Both self-update paths still download `SHA256SUMS.txt`.
- `release_asset_url(...)` still formats the same immutable GitHub release download URLs.
- The shell and PowerShell verification scripts still contain the same embedded checksum and installer filename checks.

## Fresh-Eyes Review

Re-read the new constants, both platform-specific update branches, and the tests. One portability issue was fixed during the fresh-eyes pass by gating `UNIX_INSTALL_ASSET` to tests plus Unix targets, matching the production use site and avoiding an unused private constant on Windows builds.

## Verification

- `rustfmt --edition 2024 --check src/update_check.rs`
- `git diff --check -- src/update_check.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib update_check::tests::`

## Verdict

PRODUCTIVE
