# Raw-mirror budgets and automatic retention

Automatic mirror maintenance preserves source evidence under the policy below.
It does not enable automatic age-based deletion of canonical conversations;
`cass forget` remains an explicit operation with a read-only preview default.

CASS retains the existing `whole_blob_v1` and `fixed_chunks_v1` formats. JSONL
and NDJSON transcripts use 256 KiB chunks once they reach 256 KiB. Complete
chunks are content-addressed and reused; an append replaces only its incomplete
tail and adds new chunks and a manifest. Capture rechecks source stability and
reads at most the source length admitted at the beginning, rather than chasing
an actively appending writer to EOF.

## Defaults and configuration

All byte settings are unsigned decimal byte counts, not strings with unit
suffixes. Invalid settings refuse new capture rather than silently removing a
limit. Settings are also read from the normal dotenv configuration.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CASS_RAW_MIRROR_MAX_BYTES` | `8589934592` (8 GiB) | Maximum admitted mirror file bytes, including blobs, manifests, cache, staging and audit files. |
| `CASS_RAW_MIRROR_MIN_FREE_BYTES` | `2147483648` (2 GiB) | Filesystem free-space reserve for other work. Set higher for large archives and rebuilds. |
| `CASS_RAW_MIRROR_KEEP_LATEST` | `2` | Minimum snapshots kept per exact source identity; must be at least one. |
| `CASS_RAW_MIRROR_SAFETY_HOLD_DOWN_SECS` | `21600` (6 hours) | Younger snapshots are never automatically retired. |
| `CASS_RAW_MIRROR_KEEP_TAGS` | `keep,pinned,recovery` | Comma-separated conversation tags that protect linked manifests. An empty value explicitly disables tag-based protection, not the other protections. |
| `CASS_RAW_MIRROR_AUTO_PRUNE` | enabled | `off`, `false`, `no`, or `0` disables automatic retirement, not capture admission. |
| `CASS_RAW_MIRROR` | enabled | `off`, `false`, `no`, or `0` disables capture and automatic retirement; existing copies remain. |

A zero byte cap admits no new file data; already-preserved unchanged content can
still be returned without writing it again. Raising a limit is not a disk-space
reservation. The free-space check is repeated during staging, but unrelated
processes can still consume space concurrently. The normal indexer disk-headroom
checks remain authoritative and are never bypassed.

## Normal indexing

The `cass index` CLI runs maintenance during its observed startup preflight,
after acquiring the index-run lease and starting progress observation and the
watchdog, and before the disk-headroom check. Maintenance reuses the held index
lease and then acquires the mirror mutation lock. The index lease stays held
through indexing; maintenance does not acquire or release a second index lease.
A busy mirror lock, corrupt evidence, unavailable tag database, or failed
maintenance operation is diagnosed on stderr and does not itself prevent
indexing from attempting its existing preflight.

`--data-dir` and the existing `--db` data-directory resolution are honored.
Automatic retirement is conservatively skipped for a custom database basename:
the existing prune keep-tag query uses `agent_search.db`, so consulting it for a
different archive would be unsafe. New capture remains budget-limited.

A long-running `index --watch` receives maintenance at startup, not inside a
live index transaction. The capture budget remains active throughout watch mode.
Restarting watch performs another safe retention pass. Library callers can use
`raw_mirror::maintain_before_index` before acquiring their index lease; merely
calling the library indexer does not invoke the binary's startup hook.

## Shared stores and durable session exclusions

An applied workspace or age forget can exclude one session from a shared
OpenCode or Devin SQLite store while its retained siblings continue updating
in the canonical archive. The durable exclusion survives changes to either
session, incremental scans and full scans. Source files remain untouched.

Indexing suppresses new raw captures of a shared container while any durable
session exclusion refers to it. This covers the physical database, virtual
database/session paths, resolved aliases and SQLite `-wal`, `-shm` and
`-journal` sidecars. The check applies before parsing and to captures of kept
siblings after parsing. A verbatim shared snapshot cannot leave out only the
excluded session.

Existing captures remain. Inspect them with the separate `cass mirror prune`
preview and review the retention policy before applying it; a shared capture
can also preserve retained sessions or provider data. No background deletion
of those captures is introduced by forgetting a session.

`cass forget --list --json` shows durable exclusions. Use `--reinclude` with
an exact stored `--source-path` and optional `--source-id`, `--agent` and
`--external-id` selectors to preview a release. After applying the reviewed
release, `cass index --full` can restore that session from its preserved source.
Active workspace/path rules and other durable exclusions still apply. New
shared-container captures become eligible only after every relevant durable
identity exclusion is released. Ordinary-file size/mtime stamps retain their
existing behavior.

## What automatic retirement may remove

After the keep-latest and safety hold-down protections, a manifest may retire
only when a newer, retained capture with the same provider, source
ID, origin kind, origin host and original-path identity preserves every one of
its source bytes as an identical prefix. Both captures' referenced blob hashes
are verified first. Equal complete chunks can be compared by their verified
content addresses; changed tails and old whole-file captures are compared with
bounded buffers. This also works when the original upstream file is gone.

A truncation, rewrite or origin mismatch is not proof of preservation. Such old
copies remain. Nonstandard verification/recovery records, tagged copies,
unreferenced blobs and abandoned staging evidence are not automatically
selected. The operator's explicit `cass mirror prune` policy remains separate
and can authorize more destructive retention than automatic maintenance. An
unscoped explicit prune removes `tmp/capture.*` staging left by killed captures
once nothing has written it for an hour. Under the mirror lock that prune
holds, no capture can own it.

Before retiring a manifest, its database links are merged into its retained
successor. The existing pruner records and fsyncs the exact intent before any
deletion, removes manifests before newly unreferenced blobs, and records each
completed deletion. Its reason records identify the retained successor and
preserved prefix length/hash. Shared chunks remain until all referring manifests
are retired. Repeated no-op maintenance does not append audit records.

## Disk pressure and recovery

The cap is a bound on *new capture admission*, not permission to destroy pinned
or unique evidence. An existing mirror may already exceed a newly configured
cap; protected data, recent snapshots or historical recovery evidence can also
prevent the desired retained size. In that case capture pauses instead of
continuing to grow or evicting the last recovery copy. A failed staged capture
cleans up its own temporary files without publishing partial manifests.

A `raw_mirror_capture_paused` diagnostic reports current mirror usage, the cap,
available space, the reserve and requested bytes. Indexing can continue without
that new independent raw copy when its own storage requirements are met. This
is not the same as declaring the disk healthy: a genuinely space-starved index
or rebuild can still fail its unchanged safety check.

Inspect `cass mirror prune --help` and run the desired prune policy without
`--apply` first. Review pins, hold-downs and recovery requirements before applying
it. `cass mirror prune --keep-latest 1 --safety-hold-down 6h` retires every
older capture of a source file, including divergent history that automatic
retention keeps, but never the newest capture of any source file.
Alternatively, free unrelated space or provision more storage. Automatic
maintenance can reclaim eligible old prefixes before the next index preflight;
capture resumes when a new copy fits. During a capture pause, newer source bytes
are not independently preserved against upstream deletion.

## Capture reports in status and health

`cass status --json` and `cass health --json` include `raw_mirror_capture`, and
their human output warns about a recorded capture pause. These reports keep the
current command's configuration separate from its last recorded index run:

| Field | Meaning |
| --- | --- |
| `capture_enabled`, `configuration` | The current process's capture switch, byte cap and free-space reserve. An invalid byte setting is reported explicitly. |
| `last_run.capture_enabled`, `last_run.max_bytes`, `last_run.min_free_bytes` | The configuration at the recorded run's start. It can differ from the current command's environment. |
| `last_run.capture_paused` | At least one capture was refused by the budget during that run. This is an observation of a refusal, not a live admission check. |
| `last_run.skipped_sources` | Distinct `(provider, source_id, source_path)` identities with at least one refused capture attempt. A later retry can succeed, so this is not an inventory of sources currently missing raw copies. |
| `last_run.pause_attempts` | Number of refused attempts, including retries of the same source identity. |
| `last_run.requested_reservation_bytes` | Sum of the storage reservations requested by refused attempts, including retries and overhead. These are neither source-file bytes nor unique bytes lacking a raw copy. |
| `last_run.last_pause` | Mirror usage, cap, available space, reserve and requested reservation at the most recently recorded refusal. It is `null` when no refusal was observed. |
| `last_run.completed` | Whether the run's capture accounting finalized. It does not indicate whether indexing succeeded. `false` can describe a running or interrupted index. |

The `started_at_ms`, `updated_at_ms` and optional `finished_at_ms` timestamps
identify when the observation was made. Raising the cap after a paused run does
not rewrite that history: `status: "last_run_paused"` continues to describe the
recorded run. Likewise, `last_run_no_pause_observed` does not prove that a new
capture would fit, that every source has a raw copy, or that indexing succeeded.
Capture being disabled now has the explicit status `capture_disabled`.

At run start, after acquisition of the index lease, CASS replaces the prior
observation with a new incomplete receipt. The first refusal is persisted
immediately. Further refusals update that receipt at most once every 30 seconds,
and an orderly exit writes the final counts. An interrupted run can therefore
leave partial counts that omit refusals since its last checkpoint. Treat counts
from an incomplete run as lower bounds on that run's refusals.

Distinct-source accounting retains at most 65,536 hashed identities in memory.
If that limit is exceeded, `skipped_sources_exact: false` marks the distinct
count as a lower bound. `counters_exact: false` marks saturation of the attempt
or reservation-byte counters. An exact flag describes the recorded observation;
it does not make an incomplete run complete. Human warnings include the relevant
“at least” qualifiers.

The receipt is `raw-mirror-capture.json` in the selected data directory, outside
the mirror's byte budget. It contains no source paths, source identities or
content. Status and health accept only regular receipt files up to 8 KiB and use
bounded reads. They do not scan the mirror, open the archive database for this report, probe current
free space, or create or rewrite the receipt. Missing, invalid, oversized or
unreadable receipts yield `last_run: null`, with the cause in `receipt_status`;
an unavailable observation is not an all-clear. When capture is enabled and its
configuration is valid, the report's status is `unknown` in this case.

For a run with refusals, stderr contains one immediate
`raw_mirror_capture_paused` JSON event and one `raw_mirror_capture_summary` event
when accounting finalizes, instead of a budget warning for every source. The
immediate event also makes a pause visible during a long-running watcher. Other
capture failures retain their diagnostics. A failure to persist the receipt
produces a bounded diagnostic while indexing continues; status may then show an
older or unavailable observation.

## Sizing the budget and capturing existing sources

A busy multi-agent host can have more source data than the default 8 GiB cap
can preserve even once. Size the cap from recent existing mirror usage, sources
still awaiting their first copy, expected growth between maintenance runs, and
headroom for staging, manifests and retained history. Chunk reuse reduces some
duplication but does not make raw storage free. The usage in `last_pause` is a
dated observation, so account for changes since that refusal.

Also leave filesystem space for the canonical archive, search indexes, rebuilds
and unrelated work. `CASS_RAW_MIRROR_MIN_FREE_BYTES` protects a separate reserve;
raising `CASS_RAW_MIRROR_MAX_BYTES` neither creates storage nor relaxes that
reserve or the indexer's disk checks. Retention protections can keep old evidence
above a newly reduced cap, so do not assume a smaller cap will reclaim space.

For example, after confirming that a 32 GiB mirror budget and the free-space
reserve fit the host, retry a full scan with the cap expressed in bytes:

```bash
CASS_RAW_MIRROR_MAX_BYTES=34359738368 cass index --full
```

This can capture eligible sources that still exist and were previously refused.
It cannot recreate source bytes that their provider has already deleted; parsed
archive rows are not a substitute for the original raw file. Existing exclusion
and retention rules continue to apply.

The inline environment setting applies only to that command. Persist the chosen
value in the scheduler or service environment, or the normal dotenv
configuration, for future index runs. Restart an existing watcher with that
environment so its future captures use the chosen budget. Run status or health
with the same configuration when comparing their current settings with the
historical receipt.

## Regression coverage

`raw_mirror::store::policy::tests` covers append-heavy captures, concurrent cap
admission, source growth during capture, staging cleanup, unchanged cache hits,
old/new codec readability and disk-pressure resumption.

`raw_mirror::store::policy::retention::tests` covers 80 repeated pre-index/capture
intervals, keep-latest, hold-down pressure, pinned tags, divergent history,
origin separation, upstream disappearance, corruption, orphan/recovery evidence,
lock exclusion, reclamation, DB-link transfer, audit ordering and no-op stability.
Binary tests in `raw_mirror_cli` check command and archive-directory scoping.

The library filter `raw_mirror::index_maintenance::tests::gh542_` covers durable
first/final observations, throttling, distinct identities and retries, concurrent
accounting, bounded counters, incomplete runs, optional write failures and
read-only rejection of invalid, oversized, symlink and FIFO receipts.
The integration target `e2e_codex_exclusion_capture` contains
`gh542_capture_budget_pause_is_bounded_visible_and_recovers_without_losing_search`:
it exercises real budget refusals in batch and streaming indexing, bounded stderr,
status and health visibility, unchanged search results, and successful raw
capture after raising the budget for still-existing sources.
