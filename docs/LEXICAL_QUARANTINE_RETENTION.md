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
exist. Ordinary inspection does not open the canonical database, validate lexical index
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

Each artifact also includes `failure_evidence`: metadata-only pointers to known
checkpoint, finalization-failure, generation-manifest and index-manifest files.
Each pointer has `kind`, `relative_path`, `size_bytes`, and `modified_at_ms`.
The paths remain relative to their bundle, including the `quarantine/` wrapper
for an interrupted cleanup. No diagnostic contents are opened or parsed, and a
failure record in the current live index is not attributed to every candidate.
An empty catalog does not establish absence when `size_complete` is false.

Human inspection and cleanup preview show tree age and these evidence locations.
An incomplete size is labeled unknown instead of being presented as zero; an
incomplete aggregate is not a total. Paths and diagnostic reasons are escaped
for terminal output. Multiple ordinary evidence bundles produce a warning that
retention does not repair the underlying rebuild-certification failure.

Exit 0 means a complete inventory, including an empty inventory. An incomplete
inventory is still printed as a complete JSON document with `complete: false`,
zero eligible bytes and exit 8. Invalid arguments/configuration use exit 2,
missing paths use exit 3, and other I/O failures use exit 9. Check both the exit
status and `complete`, rather than interpreting an incomplete list as empty.
Ordinary invocation remains read-only. Explicit cleanup requires the separate
preview-and-approval operation below.

## Explicit cleanup without starting an index run

Preview the current policy and review its eligible bundles:

```sh
cass lexical-quarantine --data-dir /path/to/cass-data --plan --json
```

The result contains `inventory` (the ordinary inventory schema), the resolved
`data_dir`, and `approval_fingerprint`. Incomplete or disabled inventories have
no approval fingerprint. Preview does not create or acquire a lock and can run
while an indexer holds its lock. It never creates a missing data directory.

After reviewing a complete preview, copy its entire fingerprint:

```sh
cass lexical-quarantine --data-dir /path/to/cass-data \
  --apply 'lq-cleanup-v1-<64-lowercase-hex-digits-from-the-preview>' --json
```

`--apply` requires a value; a bare flag, malformed approval, or combining it with
`--plan` is rejected by argument parsing. This is not an override of retention:
the newest bundle, recent evidence, unknown layouts and the operator opt-out
remain protected. No database, model, index rebuild or disk-headroom preflight
is started. The existing permanent `index-run.lock` may be created if absent,
but is never truncated, renamed or removed; heartbeat bytes remain unchanged.

Apply nonblockingly acquires that same lock used by normal indexing, rebuilds
the complete bounded inventory, and compares its approval before any quarantine
mutation. Approval binds the resolved data directory, observed root identities,
all recognized bundle metadata and all retention decisions. It does not expire
merely because a second passes, but newly eligible evidence, changed/new/removed
bundles or a changed newest selection require another preview. Non-Unix root
identity uses creation time; unavailable creation time refuses a plan. The token
is a plan identifier, not a secret or a substitute for filesystem permissions.
It is metadata-based change detection, not cryptographic content authentication
or protection against hostile ancestor replacement.

Every target still passes the existing identity, metadata and age recheck before
the unchanged rename/remove routine runs. The shared deletion path also verifies
that the target is still a selected member of a complete, enabled inventory and
rechecks the protected survivor's metadata before each mutation. Missing,
changed, unreadable or ambiguous surviving evidence stops further reclamation;
a stale cleanup plan cannot knowingly discard the last available candidate.
These checks also apply to automatic startup retention. Execution stops on the
first failure. This is not atomic protection against non-cooperating writers.
The complete JSON report includes `selected_bundles`, `retained_bundles`,
`attempted_bundles`, `removed_bundles`, `removed_logical_bytes`, `errors`, and
`complete`. An attempted bundle may have been renamed or partly removed even if
it is not counted as fully removed. No rollback or immediate free-space gain is
claimed. If report output fails, cleanup may already have happened: inspect
again rather than blindly replaying an approval.

Exit 6 means approval changed; 7 means the indexer lock is busy; 8 means the
inventory or execution is incomplete. Invalid approval/disabled retention uses
exit 2, missing paths 3, and I/O/report-write failure 9. A disabled preview can
still exit 0 with no approval. A successful apply with no eligible bundles is
an explicit no-op. Reusing a pre-cleanup approval after removals fails rather
than silently approving the remaining inventory. Obtain a new preview after
partial execution. Plain inspection and automatic startup retention keep their
existing behavior; this command does not change `diag` or doctor schemas.

## Automatic retention during normal indexing

Normal index startup invokes retention through the existing exclusive
`index-run.lock` sweep, **before disk-headroom preflight**. It uses one policy
across the data directory and the versioned index's parent (`index/`):

* Keep the newest normal bundle containing recognized rebuild evidence, or the
  best surviving interrupted bundle when no normal evidence remains.
* Keep every bundle with any entry modified within the last six hours, or whose
  age is unknown/future-dated.
* Reclaim older recognized bundles only after the entire inventory completes and
  each selected target passes a second metadata/identity check under that lock.

The regular staging sweep remains separate; quarantine names were not added to
its broad prefix allowlist. A target is first moved into a newly reserved sibling
`.lexical-rebuild-reclaim-<suffix>/quarantine`, then removed. Interrupted deletions
are recognized on later startups; recent partial deletions wait for the same age
threshold. Empty reservations and `index/` directory shells cannot displace
actual evidence from the newest retained slot. At least one regular file under
`index/`, or a retained checkpoint, is needed to occupy that slot; a zero-length
regular file can still be diagnostic evidence. A failed mutation stops the
quarantine sweep and logs a warning.
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
cooperative scan deadline. Candidate and survivor rechecks share one additional
budget of the same size across the whole sweep. The survivor is re-observed for
each target, so a large survivor can exhaust that budget and stop a sweep early;
no per-target allowance is renewed. An incomplete/unreadable/over-budget initial
inventory authorizes **no**
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
cargo test --locked --test cli_lexical_quarantine_cleanup
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

Do not treat source/blob verification or patch replay as native qualification.
