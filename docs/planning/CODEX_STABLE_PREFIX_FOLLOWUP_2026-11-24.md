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


## Budget raw-mirror capture without another full-prefix copy

The earlier raw-mirror adapter reconstructed a second full admitted prefix in
the global temporary directory before the raw retention/free-space policy
could refuse it. A paused capture could therefore consume B extra bytes even
when its configured raw budget was zero.

The raw adapter now carries the typed source receipt into the existing
budgeted chunk writer. It opens the original source through the existing
regular-file, no-follow, nonblocking opener, validates that retained descriptor,
rewinds after validation, and reads exactly B bytes. It hashes the bytes
actually streamed before any content-addressed blob or manifest is published,
then validates the held descriptor and named source again. The streamed hash
rejects a mixed capture even if a writer restores the original bytes before
the final source check.

New staging writes retain the existing per-write retention/free-space
accounting. Verified content-addressed chunks and an unchanged manifest can
still be reused without a new write reservation. Prefix captures neither read
nor populate the whole-source cache. The parser's immutable admission view,
ordinary raw snapshot metadata guards, source exclusions, physical sensitive
store checks, inactive EOF policy and abandoned-tail recovery remain unchanged.

Regressions include real append and descriptor-substitution cases, a
deterministically coordinated in-place mutation and restoration during chunk
capture, and isolated child processes whose global temporary path is an
existing regular file. Those children cover zero retention budget, impossible
free-space reserve, successful exact-B capture, and read-only reuse after later
appends. A fresh child completion receipt and exact one-test result prevent
an accidentally empty child test selection from satisfying the regression.
These new native tests have source review only at this publication boundary.

## Exact-366 native execution attempt

A later authorized DSR attempt used the exact
[366a6e5](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/366a6e5903ae74524c483f05cc9bd9c887b2059f)
source, unchanged nightly-2026-08-31, genuine DSR 0.2.3 at
ed04eee4a02e510c66762cdbd720aa20ef359b86, and pinned Franken Agent Detection
a0dd013a8c1e374d6a80949637e5e51cbb45550b. Its independent manifest froze 44
source/input files; their hashes were unchanged after execution.

Cargo metadata verified the parser pin. Dependency compilation reached FAD,
then the two GiB available-memory reserve stopped the owned process group
after 63.52 seconds. Available memory fell from 5,879,764 KiB to a sampled
minimum of 2,044,232 KiB. Peak owned aggregate RSS was 423,120 KiB and VAS was
1,781,600 KiB. The child received SIGTERM; the configured check returned 125.

Unlike the earlier interrupted receipt collection above, this attempt's
terminal DSR receipt was captured: outer exit 1, status failed, one observed
and executed check, zero passed. **Zero test binaries executed.** This is
dependency-build evidence, not CASS typechecking or a test result for either
the baseline or the subsequent edits. No dependency, gate, memory reserve or
toolchain pin was weakened.

[The complete native-attempt record](CODEX_STABLE_PREFIX_NATIVE_ATTEMPT_2026-11-24.json)
preserves the decoded log, terminal receipt, resource samples, guard scripts,
tool provenance and exact input manifest. Its Git blob is
5ff8f9ad493221f05bf9b5ecc09a9b382ebc4b3a and its SHA256 is
bb0d55e05bab76f18aba62f0efb70c9346a480d24fcf666a3b7bff0dfce89e4b.


## Continue healthy startup sources after a canonical-history refusal

One Codex rollout that disagreed with retained canonical history previously
terminated the startup consumer, preventing later healthy sources from
persisting. Storage now identifies three deliberate disagreements with a
typed `CodexCanonicalRefusal`: a required prefix protocol is missing,
canonical history extends beyond the admitted projection, or an occupied
canonical message index disagrees with the incoming projection. Its identity
contains the source path, external ID and canonical conversation ID; its reason
contains no transcript payload.

The consumer isolates this type only for one Codex conversation with matching
source path and external ID. Textual lookalikes, malformed proofs or metadata,
native database errors, unrelated I/O and mixed packets remain terminal.
The refused source retains its invalidated prefix proof and receives no
completion receipt or successful-source count. Provider/root coverage remains
incomplete, while subsequent healthy sources can receive their own canonical
completion receipts. This is a canonical reconciliation diagnostic, not an
active-writer diagnostic.

Earlier persistence slices may already have committed before a later slice
refuses the source. The consumer therefore permanently detaches its borrowed
lexical writer, switches subsequent ingestion to the existing deferred
authoritative-rebuild strategy, and atomically persists an incomplete
`CanonicalMetadataRepair` checkpoint before continuing. The checkpoint writer
syncs the file and parent directory. If that write fails, the error remains
terminal and the current byte reservation is released.

The existing finalization path drops the actual lexical writer and rebuilds
from canonical rows. The persistent checkpoint preserves repair debt across
interruption after healthy sibling receipts have committed. The implementation
does not infer rollback of earlier slices or publish the abandoned staged
lexical additions. The streaming-disabled boundary adapter also preserves the
deferred strategy and cannot hand that writer to its fallback pass. A failed
writer close takes precedence over a typed refusal, preventing source isolation
from hiding cleanup failure.

Five new host regression entrypoints cover bad-first/bad-middle order for
local and remote sources; source-commit interruption after the later healthy
source; a real late-slice refusal after an earlier committed insert; checkpoint
rename failure; and strict typed/singleton identity classification including
writer-close precedence. Four existing storage tests gain typed reason and
identity assertions. These are authored native regressions with independent
source review; no runtime pass is claimed.

### Remaining isolation work

This increment is limited to source-exclusive startup boundaries. A canonical
refusal in a mixed watch batch remains terminal because earlier source/slice
outcomes and derived-index work can already have committed. It needs explicit
partial-outcome accounting before safe per-source continuation. This change
adds no unsafe retry-by-bisection or historical canonical backfill. The broader
stable-prefix bead remains open.

## Final scoped formatter and scanner attempt

A separate genuine DSR 0.2.3 run used the exact eleven final Rust files from
the raw-budget and startup-isolation increments. Independent Git blob and
SHA256 checks matched the reviewed postimages, and every after-run source hash
was unchanged. It ran no compiler or native test.

Both checks were stopped by the unchanged two GiB available-memory reserve.
Rustfmt ran for 2.117 seconds and emitted two style differences before SIGTERM:
one existing expression in `stable_prefix.rs` and one newly added test
expression. It did not finish checking all files, so this is neither a
formatting pass nor an all-source parsing result. The UBS stage verified its
pinned v5.4.4 runner and contract and enumerated the eleven targets, then
stopped after 0.614 seconds before a completed AST positive control or scanner
result.

The terminal receipt records two executed checks, zero passed, both check
exit codes 125, and status failed. No automatic formatting, gate weakening,
compiler retry or additional scanner run followed.
[The complete scoped-attempt record](CODEX_STABLE_PREFIX_SCOPED_ATTEMPT_2026-11-24.json)
preserves both full logs, the receipt, resource samples, exact file identities
and validation scripts. Its Git blob is
058d190543ff0219538281ca9e63b6af96f76684 and its SHA256 is
2932a66fd70e9fd14d16dceb1046b27375f55906d7ab3c8177d32eb329c1fae6.


## Direct publication and rebased validation

The raw-budget increment was published directly to main as
[308378f](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/308378f8d940d49e736429cddfb3ccca02cf99ce).
Four intervening upstream commits through c2c5cc529a3b9e89ee35d88b8cdc935c979f536e
were preserved. Only the startup patch's SQLite path overlapped them.
Its four reviewed edits matched the newer source exactly once; reversing those
edits reproduces the entire upstream file byte for byte. Independent root
reconstruction matched the rebased blob
e2e70fb76dfe3e6e3d6363802797c15fc2dab643. The other three startup files were
unchanged by the intervening commits.

A new genuine DSR run froze the seven published raw files and four startup
files, including the rebased SQLite source. Both current UBS gate functions
were reproduced byte for byte, and all sixteen input hashes, including the
eleven Rust targets, were unchanged after execution.

The scoped rustfmt and UBS checks again stopped at the unchanged two GiB
available-memory reserve. Rustfmt ran 5.216 seconds, emitted the two previously
recorded style differences, and did not complete all-file checking. UBS setup
ran 0.842 seconds and did not complete its AST positive control or scanner.
The terminal receipt records outer exit 1, two executed checks, zero passed,
and supervised exits 125 and 125.

The separately prepared native harness was not launched. Its preflight
encountered ENOSPC while recording the result; a read-only observation then
found zero workspace bytes free and 4,156,468 KiB available memory, below the
authorized six GiB starting threshold. No source was modified and no further
compiler, formatter or scanner retry was performed.

[The complete rebased-attempt record](CODEX_STABLE_PREFIX_REBASED_ATTEMPT_2026-11-24.json)
contains both full logs, the terminal receipt, resource observations, exact
source identities, unchanged gate functions and the native-preflight failure.
Its Git blob is d13ceab6723f54b6f479c2e16f6a67213b1965c5 and its SHA256 is
14325d533b042e9e1133c7a0eb855104a43fb82dc9cdf27a10ccfefe3846e6d9.

## Qualification boundaries

These changes have source review and exact source-identity checks, not final
native compilation or runtime qualification. The original frozen candidate's
earlier passing tests do not qualify these follow-ups. No release, real-watch
latency, cross-platform or broad-bead completion is claimed.
