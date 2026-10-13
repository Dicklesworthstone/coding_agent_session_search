# Failed lexical rebuild evidence: inventory and retention

GH #523 reports multi-gigabyte `.lexical-rebuild-quarantine-*` directories left
by failed/resumed rebuilds. These directories are separate from ordinary orphan
staging, published lexical generations, and publish backups. Repeated quarantine
creation still means rebuild certification failed: retention does not diagnose
or fix that original failure.

## Inspect without opening the archive

```sh
cass lexical-quarantine --data-dir /path/to/cass-data --json
cass lexical-quarantine --data-dir /path/to/cass-data
```

`--robot` is an alias for `--json`. The data directory is required and must already
exist. The command does not open the canonical database, validate lexical index
payloads, load models, create directories, acquire mutation locks, or delete
anything. It can inventory evidence while the archive itself is unreadable.
The command is registered in the ordinary CLI's external-command discovery,
help, and completion tree. Existing `diag --quarantine` and doctor schemas are
unchanged; use this command for the rebuild-specific inventory.

JSON reports each bundle's path, logical regular-file bytes, tree age,
completeness, and disposition: `retain_newest`, `retain_recent`, `retain_disabled`,
`eligible`, or `inspect`. `eligible` describes policy only, not a deletion
approval. The metadata fingerprint detects observed tree changes; it does not
hash file contents, certify freshness, or authorize an apply operation.

Exit 0 means a complete inventory, including an empty inventory. An incomplete
inventory is still printed as a complete JSON document with `complete: false`,
zero eligible bytes and exit 8. Invalid arguments/configuration use exit 2,
missing paths use exit 3, and other I/O failures use exit 9. Check both the exit
status and `complete`, rather than interpreting an incomplete list as empty.
There is no `--apply` flag on this read-only command.

## Automatic retention during normal indexing

Normal index startup invokes retention through the existing exclusive
`index-run.lock` sweep, **before disk-headroom preflight**. It uses one policy
across the data directory and the versioned index's parent (`index/`):

* Keep the newest normal bundle containing recognized rebuild evidence.
* Keep every bundle with any entry modified within the last six hours, or whose
  age is unknown/future-dated.
* Reclaim older recognized bundles only after the entire inventory completes and
  each selected target passes a second metadata/identity check under that lock.

The regular staging sweep remains separate; quarantine names were not added to
its broad prefix allowlist. A target is first moved into a newly reserved sibling
`.lexical-rebuild-reclaim-<suffix>/quarantine`, then removed. Interrupted deletions
are recognized on later startups; recent partial deletions wait for the same age
threshold. Empty reservations cannot displace actual evidence from the newest
retained slot. A failed mutation stops the quarantine sweep and logs a warning.
The canonical archive, live lexical index and publish backups are outside this
policy. No new background task or timer is installed.

To preserve all rebuild evidence during an investigation:

```sh
CASS_LEXICAL_QUARANTINE_RETENTION=0 cass index --data-dir /path/to/cass-data
```

The default is enabled; `1` explicitly enables it. Any value other than `0` or `1`
refuses automatic retention with a warning. The inventory command reports the
same setting. Disabling this policy does not disable the older orphan-staging
sweep or the separate raw-mirror retention policy.

## Ownership, bounds and limitations

Only exact generated names with six ASCII alphanumeric suffix characters match.
A normal bundle may contain `index/`, `.lexical-rebuild-state.json`, or both;
checkpoint-only failures remain diagnostic evidence. Unknown top-level entries,
symlinks and special files require inspection and disable the complete sweep.
On Unix, cross-device entries and hard-linked regular files are also refused.
The data/index roots must be real directories, not symlinks. Their ancestors must
be controlled by the operator: metadata rechecks are not an atomic defense
against a hostile process replacing path components during filesystem calls.

Inspection bounds the combined roots to 16,384 entries, 1,024 bundles, 65,536 tree
entries, 4 MiB of retained relative-path bytes, depth 32, and a five-second
cooperative scan deadline. Candidate rechecks share one additional budget of the
same size. An incomplete/unreadable/over-budget inventory authorizes **no**
quarantine deletion, even if earlier entries looked eligible. Path bytes and
file lengths are not total process-memory or allocated-disk-block measurements.
Synchronous filesystem calls and recursive deletion are not forcibly preempted.

This is conservative retention, not a hard disk quota: the newest bundle, recent
bundles, and unknown evidence may still occupy substantial space. Logical bytes
reported removed do not guarantee an immediate free-space increase (open handles,
filesystem snapshots and allocation details can differ). No production-archive
space-saving or throughput measurement is implied by the implementation.

## Qualification

The new inventory, retention, startup-wrapper and CLI regressions use isolated
filesystem fixtures. They have **not been compiled or executed in the authoring
environment**. Before release, run the shipping-lockfile suites and repository
gates, including:

```sh
cargo test --locked --lib lexical_quarantine
cargo test --locked --lib staging_reclaim
cargo test --locked --bin cass lexical_quarantine_cli
cargo test --locked --test cli_lexical_quarantine
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

Do not treat source/blob verification or patch replay as native qualification.
