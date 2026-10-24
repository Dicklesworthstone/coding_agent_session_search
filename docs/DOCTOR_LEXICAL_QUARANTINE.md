# Doctor: failed lexical rebuild evidence

`cass doctor quarantine` is the native doctor entry to the same inventory,
preview, and approved reclamation used by `cass lexical-quarantine`. It runs
before ordinary doctor/archive initialization. An unreadable canonical database
is not a reason to open or repair that database just to inspect derived evidence.
Use an explicit existing data directory; no default archive is selected.

```sh
cass doctor quarantine --data-dir /path/to/cass-data --json
cass doctor quarantine --data-dir /path/to/cass-data
cass doctor quarantine --data-dir /path/to/cass-data --plan --json
```

Put `doctor quarantine` before its options. `--robot` is an alias for `--json`.
`cass doctor quarantine --help` describes the actual execution parser. The
standalone command's help also points to this spelling. The ordinary `doctor
--help` flag-based command tree is unchanged.

Inventory reports bundle paths, logical bytes, tree age, retention disposition,
completeness, and metadata-only failure-evidence locations. Diagnostic contents
are not parsed or copied into output. Human paths and reasons are escaped. A
partial scan is not an empty inventory and does not authorize any cleanup.

## Apply a reviewed preview

Copy the full `approval_fingerprint` from `--plan` after reviewing its inventory:

```sh
cass doctor quarantine --data-dir /path/to/cass-data \
  --apply 'lq-cleanup-v1-<64-lowercase-hex-digits-from-preview>' --json
```

This is not a `--yes` override or a doctor repair. No `--fix`, `--force-rebuild`,
`--db`, or generation-cleanup approval is accepted. The shared executor takes
`index-run.lock` nonblockingly, re-inventories, compares the preview fingerprint,
and uses the existing retention and evidence protections. A plan from the
standalone command is interchangeable with a doctor-quarantine plan for the
same unchanged archive. Changed inputs require a new preview.

The newest diagnostic bundle and evidence touched within six hours remain
protected. Unknown layouts, symlinks, incomplete scans, changed survivors, and
operator-disabled retention cannot be overridden by this command. Interrupted
cleanup retains the existing recognizable namespace. Linux mount and exact-entry
removal safeguards remain in the shared core, not a second doctor implementation.
The original `CASS_LEXICAL_QUARANTINE_RETENTION=0` opt-out is respected.

Apply may create the permanent lock file, but never truncates or deletes it.
Read-only inspection and preview do not create it. An active indexer prevents
apply while still allowing read-only inspection. A partial cleanup is not rolled
back: inspect again and obtain a fresh plan before retrying. Reported logical
bytes removed are not a guarantee of immediately available disk space.

Exit codes match the standalone command: 0 complete, 2 invalid arguments or
configuration/disabled apply, 3 missing paths, 6 changed approval, 7 busy lock,
8 incomplete inventory/execution, and 9 other I/O or output failures. JSON error
and success output retain the same schemas as the standalone command.

This adds the explicit `doctor quarantine` surface. It does **not** change the
existing `doctor check`, `doctor cleanup`, or `diag --quarantine` report schemas.
In particular, it does not apply the separately prepared monolith integration.
Repeated evidence still calls for investigation of rebuild failure or interruption;
reclamation does not itself repair a failed rebuild.

## Regression qualification

Fresh-process fixtures exercise the real doctor spelling, invalid database
isolation, inventory metadata/privacy, shared approvals, index-lock contention,
retention opt-out, unknown contents, old/interrupted reclamation, newest/recent
protection, symlinks, stale approval, and help/argument refusals. Aging cases are
Unix-only. Run the pinned shipping-lockfile commands before release:

```sh
cargo test --locked --bin cass lexical_quarantine_cli
cargo test --locked --test cli_doctor_quarantine
cargo test --locked --test cli_lexical_quarantine
cargo test --locked --test cli_lexical_quarantine_cleanup
cargo test --locked --lib lexical_quarantine
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

The authoring environment has no Cargo/Rust toolchain. These tests are added,
not claimed executed; source/transport checks are not native qualification.
