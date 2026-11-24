# Codex stable-prefix follow-up: admission and receipt integrity

This follows commit f9d7a0a8014e877482190f92498ff5eb28020c9a and the
[stable-prefix implementation notes](CODEX_STABLE_PREFIX_INGEST_2026-11-23.md).
It addresses remaining source-admission and integrity failures under the same
GH #559 / watch-stable-prefix-ingest-v6ysi work. The broader bead remains open.

## Source-admission fixes

### Prevent the activity probe from waiting for a FIFO writer

The strict Codex source opener already used nonblocking Unix opens, but the
earlier activity classifier still opened a pathname before checking its
descriptor type. A pathname replaced with a FIFO could therefore wait for a
writer before reaching the safe connector opener.

The advisory-lock probe now applies O_NONBLOCK to the actual Unix open,
including regular-file symlink targets, then checks the opened descriptor is
a regular file before attempting a lock. This retains ordinary symlink and
advisory-lock behavior. Absence of lock evidence does not admit a source:
the connector still performs its independent type, identity and byte checks.

The regressions use real FIFOs without writers in a child process bounded by
its parent. They cover direct and symlinked FIFOs, replacement after a regular
metadata observation, the activity-filter call path, and recovery after the
original regular file is restored. A fresh completion receipt prevents a
zero-test child invocation from satisfying the parent test. A separate control
preserves regular-file locking, symlink identity, bytes and modification time.

This bounds the FIFO writer rendezvous. It is not a universal timeout for
filesystem pathname resolution, FUSE/NFS operations, scheduling or process
startup.

### Keep nonempty active sources on the prefix admission path

A nonempty active rollout with no LF previously returned no prefix snapshot.
The public connector then delegated to its historical strict parser, which can
accept a complete JSON value at EOF without a newline. A first event-side user
prompt could consequently be delivered before its adjacent response arrived.

Such a source now returns typed UnexpectedEof and source-local retry. It emits
neither a conversation nor a completion receipt. A withheld trailing prompt
uses the same retry path. Empty and unsupported inputs retain their existing
strict routes. Inactive valid EOF and abandoned-tail recovery remain supported
through their existing inactivity policy.

The scope is the public connector router: normal host admission already
rejects this active no-LF input before parsing. The historical strict-parser
control still checks one EOF delivery with no active-prefix permission, using
the actual strict scan entry point and real enrichment.

The new public-connector regressions cover closed and held-open recent sources,
LF-only prompt completion followed by the adjacent response, stable replay
digests, inactive valid EOF and abandoned incomplete suffixes.

## Receipt-integrity fixes

### Refuse non-object durable observations

Every valid source-ledger observation is a JSON object. Scalars, null and arrays
previously looked like legacy observations with no prefix field. Admission now
rejects those roots after clearing prior thread-local state. Object observations
without a prefix field retain legacy compatibility.

This refusal does not turn serialized JSON into admission authority. Existing
typed scope, source identity, content and canonical overlap checks still apply.

### Isolate bad rows during full/rebuild preparation

Full scans and rebuilds preserve old prefix evidence while disabling reuse.
Previously, one malformed or oversized saved observation could abort this
preparation for the entire provider before any healthy source was scanned.

Preparation now handles each row independently. It bounds input before parsing,
requires an object root, and retains unusable evidence under the exact original
key using a small invalid-prefix marker. The marker is only in the producer's
in-memory map; it never replaces the original durable row. Actual Codex
admission rejects it, preserving that source's refusal/retry state while
healthy siblings proceed.

Valid prefix evidence is still invalidated for reuse and retained for overlap.
Ordinary complete observations keep their existing handling outside rebuild
preparation. Real database-read failures remain terminal. The bounded watch
cache and its exact-key fallback are unchanged; this transform receives the
uncapped canonical ledger, not the watch-cache omission sentinel.

Regressions cover malformed JSON, scalar roots, oversized input, output growth
past the byte bound, exact-key retention, valid-proof preservation and healthy
local/remote siblings through the real startup scan path.

## Keep canonical prefix history on the verified admission protocol

A previously prefix-ingested regular rollout can later become an explicitly
selected symlink. The source-ledger key includes the canonicalized path, so this
replacement can miss the old receipt. The supported strict parser preserves
the lexical path and external ID but produces no prefix metadata. Previously,
storage then selected the existing conversation and allowed its ordinary
append/replay shortcuts without checking the earlier prefix.

Canonical storage now records `/cass/codex_prefix_required: true` for new
prefix conversations and for every successfully verified existing-prefix
admission, including a no-op legacy upgrade. Admission resolves the actual
canonical conversation ID inside the transaction, then checks this requirement
before ordinary merge shortcuts. A non-prefix Codex packet is refused when
canonical metadata contains either the requirement or an older full prefix
receipt. Ordinary valid legacy packets remain supported when neither exists.

The requirement is not a publication receipt. It grants no source completion,
reuse, search publication or privacy authority. Source identity, admitted
bytes, source overlap and canonical message checks continue to apply.

The metadata read bounds the combined JSON and MessagePack representation to
256 KiB in SQL before returning either payload. Nonempty MessagePack takes
precedence; a decoding failure cannot fall back to potentially stale JSON.
The upgrade modifies existing canonical metadata, preserving unrelated fields,
and checks the output budget before writing. Marker updates, canonical writes
and any completion receipt share the existing transaction, so a later refusal
rolls them back together. Non-Codex packets return before this extra query.

Five additional native regression entrypoints cover new and older prefix rows;
JSON and MessagePack legacy upgrades with append and no-op cases; rollback of
an earlier prefix upgrade after a later same-transaction refusal; encoded
metadata bounds and strict decoding; and the real explicit-symlink producer
route through both single and batched storage entrypoints. These tests have
source review only. The earlier six canonical-overlap tests are unchanged.

### Historical migration limit

This requirement is prospective. A legacy conversation upgraded before this
correction may have prefix evidence only under an obsolete source-ledger key,
with no canonical prefix metadata. That state still needs explicit
reconciliation or backfill. This change adds no archive-open migration or
unindexed ledger scan and does not claim to repair such historical states.

### Publication and concurrent changes

The admission and rebuild fixes were committed directly to main as
[c6dfcc5](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c6dfcc5aad95a96180260debe80edfb5f93d0977).
They were applied on top of concurrent commit
[46f433f6](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/46f433f6bb9dd296dcd50b1691de26af11aa5a89).
The canonical-storage patch was separately reapplied to that commit's verified
storage blob using exact source contexts. All unrelated forget/WAL recovery
edits were retained. The final storage bytes were independently checked against
the reviewed patch and their Git object hashes.

## Restored-environment validation attempt

The environment was restored long enough to obtain the unchanged pinned
nightly-2026-08-31 toolchain and genuine DSR 0.2.3 at
ed04eee4a02e510c66762cdbd720aa20ef359b86. The unchanged Codex contract harness
ran against a frozen f9d7a0a checkout and verified the pinned FAD source.
Cargo metadata completed and dependency compilation began.

The watchdog stopped the owned compiler process group after 92.059 seconds
because host available memory crossed below the two GiB reserve:

| Measurement | KiB |
| --- | ---: |
| Initial available memory | 3,506,368 |
| Lowest sampled available memory | 2,091,008 |
| Peak owned aggregate RSS | 291,564 |
| Peak owned aggregate VAS | 2,754,200 |

The configured check returned 125 after SIGTERM. **Zero test binaries ran.**
DSR was still computing its post-run source snapshot when last observed, so
its aggregate terminal result and final receipt are unknown.

New negative and fixed source snapshots were prepared but not executed.
Rustfmt/Clippy versions were queried, and genuine pinned UBS and ast-grep
assets were acquired and verified, but this attempt produced no completed
formatter, Clippy, UBS, AST-probe, host/storage or CLI result. Root's formatter
session also has unknown terminal status. The previously recorded failed UBS
gate remains failed; no gate was weakened or waived.

The environment later became unavailable. Its loss cause is unconfirmed.
Final changes were recovered through exact source patches and Git objects.
The advisory-probe files, recovery comment and strict-parser control match their
captured pre-loss SHA256 hashes. The no-LF-only production core also matches
its captured hash; the observation-shape check was subsequently added.

The zero-boundary test file was regenerated after loss of its original body.
It preserves the intended real-connector contracts but has a different hash
from the unexecuted diagnostic snapshot. No runtime evidence transfers to it.

[The accompanying validation JSON](CODEX_STABLE_PREFIX_FOLLOWUP_VALIDATION_2026-11-24.json)
contains the captured decoded check log, tool provenance, watchdog results,
input manifests and final source hashes. The prior evidence ZIP does not
contain this newly captured attempt.

## Qualification boundaries

These changes have source review and exact source-identity checks, not final
native compilation or runtime qualification. The original frozen candidate's
earlier passing tests do not qualify these follow-ups. No release, real-watch
latency, cross-platform or broad-bead completion is claimed.
