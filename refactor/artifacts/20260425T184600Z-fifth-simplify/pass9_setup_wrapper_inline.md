# Pass 9 - Setup Wrapper Inline

## Change
- Removed the private `generated_source_name_for_host(...)` wrapper in `src/sources/setup.rs`.
- Inlined its only behavior, `super::config::normalize_generated_remote_source_name(...)`, at both local callsites.

## Fresh-Eyes Review
- Re-read the removed wrapper and both replacements.
- Confirmed the dedupe path still stores the same generated source name and derives the same case-insensitive key.
- Confirmed the non-interactive selection filter still computes a generated name before calling `source_name_key(...)`.
- Confirmed no public setup API or user-facing output changed.

## Verification
- `rustfmt --edition 2024 --check src/sources/setup.rs`
- `git diff --check -- src/sources/setup.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --lib sources::setup::tests::test_dedupe_selected_hosts_by_generated_name`

## Verdict
PRODUCTIVE: removed a private forwarding hop while preserving generated remote-source name behavior.
