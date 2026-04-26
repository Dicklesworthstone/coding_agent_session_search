# Pass 7 - Remote spec wrapper collapse

- Run: `20260426T155536Z-eighth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Wrapper Hop Collapse
- Scope: `src/sources/sync.rs`
- Verdict: PRODUCTIVE

## Change

Removed the private `remote_spec_for_scp(host, remote_path)` pass-through helper
and routed the direct SFTP/SCP call through `remote_spec_for_rsync(..., true)`,
which already represents the raw remote-spec path.

## Isomorphism Card

Preserved behavior:

- Protected-args/raw mode still formats remote specs as `{host}:{remote_path}`.
- Shell-bound mode still quotes only the remote path via
  `remote_spec_for_shell_bound_copy(...)`.
- The SFTP/SCP fallback call still gets the same raw remote spec.
- Existing raw-space behavior remains covered, and the raw apostrophe case moved
  into the remaining rsync helper test.

## Fresh-Eyes Review

Re-read every call site after deleting the helper. The old direct SCP test cases
are still represented by `remote_spec_for_rsync(..., true)`, and the false branch
continues to use shell quoting unchanged.

Yes, preservation was verified according to the skill: the surviving focused
test now pins both raw path cases and the shell-quoted path case.

## Verification

- `rustfmt --edition 2024 --check src/sources/sync.rs`
- `git diff --check -- src/sources/sync.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eighth_simplify cargo test --lib sources::sync::tests::test_remote_spec_for_rsync_quotes_only_when_needed`

