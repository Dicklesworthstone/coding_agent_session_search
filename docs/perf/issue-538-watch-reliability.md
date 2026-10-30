# Issue #538: watch indexing, raw-mirror I/O, and bounded recovery

Investigation date: 2026-10-30. Baseline: `f94ea2d3ff9bf62cce5b67f6f642947de35c4109`.
Report: [coding_agent_session_search#538](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/538).

The reporter's macOS archive contains 8,560 conversations, 683,558 lexical
documents, and an approximately 11 GB raw mirror with 20,816 files and 9,523
manifests. Standalone incremental indexing completed in 187 seconds, while the
watch service repeatedly exited 70 after approximately 300 seconds. All 4,446
samples in a five-second sample found the main thread in `lstat`, holding both
index and mirror leases.

**The original blocked pathname is not established.** The stripped stack sample
does not reveal which pathname or Rust call site was involved. These changes
correct source-level watch defects and identify in-flight
capture/admission I/O; they do not prove that the reporter's kernel wait has
been reproduced or resolved on the original machine.

## Confirmed differences and repeated work

In `src/indexer/mod.rs`, startup indexing uses `run_streaming_index`,
`spawn_connector_producer`, and `scan_with_durable_source_boundaries`; connector
activity and delivered conversations update progress while bounded batches
reach persistence. Watch callbacks use `reindex_paths_with_semantic_delta`.
Before this change, that path collected a whole root into a `Vec`, raw-mirrored
every selected conversation, and sorted it before committing the first batch.
It also omitted the connector progress callback installed by startup scanning.
A broad watch event or replay debt could
therefore create a large silent interval even when filesystem work advanced.

The locked mirror path is `policy::capture` or `merge_manifest_db_links` in
`src/raw_mirror/`. Both reach `std::fs::symlink_metadata` (`lstat` on macOS).
The previous inventory fix did not eliminate these separate costs:

- Near-cap admission required a full inventory inside an arbitrary one-eighth
  margin, even when conservative cached accounting already proved the decision.
- Inventory retained all sibling paths and held a process-wide cache mutex
  during filesystem I/O, with no cancellation or completed-operation progress.
- Verified unchanged captures still replaced `.usage-generation` and created
  temporary directories. Repeated existing DB links also rewrote the generation
  and read the same manifest twice before discovering there was nothing to do.

The near-cap defect is independently relevant; the report does not specify the
configured cap. Its 11 GB mirror alone does not establish a near-cap workload.

## Corrections and preserved invariants

Watch scanning retains a bounded payload buffer and spills larger backlogs to
an anonymous temporary file. Compact keys preserve the previous stable global
ordering by payload watermark, source path, and external ID. Discovery and
sorting finish before durable ingestion starts. Parsed-conversation raw capture
now runs for the batch about to persist; storage/index locks are released between
batches. Existing preparse primary-source capture retains its source-count
threshold and defers larger inventories. Non-UTF-8 native paths survive the JSON spool
round trip. Sort keys and native-path sidecars remain resident; the payload limit
is not a total RSS cap.

Completed filesystem operations update activity. Waiting for a lock or a
blocked syscall does not. Cancellation is checked between operations and
bounded source/spool reads and writes; a kernel-blocked syscall itself remains
subject to the existing watchdog. No stall deadline is increased or disabled.

The inventory walker keeps at most 64 directory cursors, refuses symlinks and
unreadable or non-regular entries, and releases the global accounting mutex
before filesystem work. Accounting caches cover at most 256 mirror roots.
Conservative upper bounds admit writes; walked lower bounds refuse them.
Replacement shrinkage updates those bounds. Ambiguous/expired decisions still
measure, and another writer's generation or a prune invalidates stale usage.

Unchanged verified captures and DB-link merges avoid filesystem mutations.
Actual changes retain the mutation lease, generation fence, checksums, canonical
paths, source identity/stability checks, byte cap, free-space reserve, and
publication ordering. Partial scans, cancellation, or failed lexical publication
retain replay debt and do not advance a watermark past unfinished work. An
anonymous spool is disposable; canonical transactions and source files remain
the recovery authority after interruption.

## Regression workloads

- Mirror policy: 20,816 files, 9,523 valid manifests, and 11 GiB of logical file
  bytes. Sparse abandoned staging models interrupted captures without allocating
  11 GiB physically. Twelve append/no-op cycles plus twelve near-cap refusals
  require two inventories, preserve old/new captures, and enforce the cap.
- Watch spool: 8,560 conversations and 683,558 messages, generated and consumed
  incrementally. Assertions cover ordering, payload bounds, exact counts,
  oversized-source isolation, native paths, cancellation, and cleanup.
- Live watcher (implemented, not executed in this environment): 128 real preserved sources, repeated no-op source discovery,
  new content and successive appends, duplicate prevention, and process restart
  recovery. It uses the actual watcher and searchable canonical results.
- Safety: interrupted inventories cannot populate the cache; another store can
  access accounting during a walk; shrinkage/prune/writer handoff must permit
  correct admission decisions. Existing corruption and symlink tests remain.

## Diagnose a remaining stall

The structured stall event includes `diagnostics.raw_mirror_io`, an array of
in-flight `{operation, path, elapsed_ms}` records. Inspect the stall event as
well as the separate compact `kind: "index-stalled"` exit-70 envelope. The
snapshot does not stat the suspect pathname again or require debug symbols.

For a short diagnostic run, the before-operation trace names each instrumented
capture/admission filesystem call. Status/prune operations outside these wrappers
can leave this array empty. Retain the failing source roots and retention settings:

```sh
RUST_LOG=warn,coding_agent_search::raw_mirror=trace \
  cass index --watch --watch-interval 30
```

Do not add `--json`, `--robot`, `--quiet`, or `--verbose` to that trace command:
those modes override `RUST_LOG`. To retain JSON command output, use the separate
bounded trace artifact instead:

```sh
CASS_TRACE_FILTER=warn,coding_agent_search::raw_mirror=trace \
  cass --trace-file cass-watch-538.jsonl index --watch --watch-interval 30 --json
```

## Validation commands and status

The attempted full gate used the pinned `nightly-2026-08-31` toolchain, one Cargo
job, a 128 MiB test-thread stack, and these actual selections:

```sh
source /tmp/cass-538-env.sh
GATE_RECEIPT_FILE=/tmp/cass-538-final-gate-receipt.log scripts/gate.sh --local \
  --lib-filter 'raw_mirror watch_spool watch_idle_wait watch_reindex watch_lexical reindex_paths_carries_only_new_packet_semantic_delta_messages watch_conversation_sort stall_diagnostics_tests' \
  --integration 'watch_e2e:gh538,cli_index:gh439_slow_post_publish_fts_repair_is_not_aborted_while_it_heartbeats gh439_parked_post_publish_fts_repair_still_aborts_with_the_index_stalled_envelope' \
  > /tmp/cass-538-final-gate-output.log 2>&1
```

The gate is **red**: `STAGE=fmt EXIT=0`, `STAGE=clippy EXIT=101`, and
`STAGE=lib-build EXIT=101`. Compiling dependency `asupersync` repeatedly received
SIGKILL under the 8 GiB cgroup limit; disk space was also exhausted. No application
test binary completed, so live-watch, canonical storage, CLI stall, bin, and
golden tests have no passing receipts. Checkout changes also made
`STAGE=source-stability EXIT=1`. This attempt predates the final two test-fixture
corrections; it is not validation of the final source snapshot.

### Supplemental exact-source execution

A small crate selected verbatim filesystem implementation ranges and thirteen
filesystem tests: eight GH538 regressions plus five existing generation, alias,
hardlink/symlink, and concurrent-budget tests. It also included the exact watch
spool/idle-wait code, original normalization types, and selected atomic progress
methods. Source ranges, input/output SHA256s, dependencies, and exclusions were
recorded in `raw_mirror_sources.json`, `watch_sources.json`, and
`dependency_provenance.json` under `/tmp/cass-538-exact-harness`.

This harness omits SQL pruning, automatic retention, the provider scan, canonical
transactions, lexical publication, and the real watch subprocess. Its atomic
progress carrier omits the SQL cancellation relay. No SQLite implementation is
substituted. These are focused filesystem and spool results, not a full CASS gate.

Build/run commands, with per-test process resource measurement:

```sh
source /tmp/cass-538-env.sh
export RUSTC_WRAPPER=/tmp/cass-538-rustc-wrapper.py
unset CASS_INDEX_SKIP_DISK_HEADROOM_CHECK
cd /tmp/cass-538-exact-harness
cargo +nightly-2026-08-31 test --offline --locked --lib --no-run -j1
export TMPDIR=/dev/shm/cass-538-fixtures-14e01c69a7b2
python3 /tmp/cass-538-harness-run.py /tmp/cass-538-harness-results-final
python3 extract_raw_mirror.py --verify
```

**25/25 supplemental tests passed**: thirteen filesystem tests, nine watch
spool/idle-wait tests, and three original FAD normalization tests. Each ran in a
separate measured process.
The compiler wrapper changes allocator bookkeeping and records resource usage;
it does not change Rust source or compilation options. Binary SHA256:
`4f604cabf88c7f25cdb77b4334126783ddff2a23091c80d8b26830ae66038190`.

| Workload | Observed progress | Complete test wall time | Maximum child RSS |
| --- | --- | ---: | ---: |
| 20,816-file mirror, 9,523 valid manifests, 11 GiB logical bytes | 12 append/no-op cycles, 12 quota refusals, exactly 2 inventory walks | 2.274 s | 10,112 KiB |
| 8,560 conversations, 683,558 messages | 268 ordered batches, all payloads recovered | 12.670 s | 10,112 KiB |

The mirror cycle portion took 197.747 ms after fixture creation. The spool test
retained at most 1,416,640 payload bytes against 289,116,288 bytes for the complete
normalized corpus and wrote 143,166,362 temporary bytes. Fixtures ran on Linux
**tmpfs**, with real free-space checks enabled. Tmpfs spool pages consume host
RAM outside the child's RSS. The reported child `ru_maxrss` also has the Python
launcher's pre-exec memory floor; 10,112 KiB is not a full-CASS memory measurement.
Sparse file sizes model logical mirror bytes, not physical 11 GiB I/O. These
timings are not APFS benchmarks or an observed speedup over the baseline.
The DB-link test uses a 1 MiB fixture cap
and a nonzero 64 MiB reserve; production defaults are unchanged.

Final source/environment hashes are in `/tmp/cass-538-harness-final-context.json`;
per-test logs, exit statuses, and resource measurements are in
`/tmp/cass-538-harness-results-final/resources.jsonl` and adjacent logs.

### UBS remains blocking

Identical pinned UBS v5.4.4 scans cover all six changed Rust files, with identical
`.ubsignore` and `--ci --fail-on-warning`. The gate scans whole files and includes
test assertions; it does not narrow findings to changed lines.

| Snapshot | Critical | Warnings | Info | Exit |
| --- | ---: | ---: | ---: | ---: |
| Baseline `f94ea2d3` | 75 | 16,853 | 8,481 | 1 |
| Candidate before final fixture corrections | 76 | 17,024 | 8,547 | 1 |
| Final candidate after fixture corrections | 76 | 17,032 | 8,547 | 1 |

All 75 baseline critical identities remain in the reviewed candidate; the added
critical is the idle-cancellation test's explicit failure `panic!`. The final
warning delta is 181 added/two removed: 170 added test records and eleven
reviewed production records. Ten cover guarded spool invariants, bounded
slices/clones, a fixed basename join, and lazy error formatting; the eleventh
corresponds to the existing canonical manifest-path check after its single-read
refactor. No production critical site was
introduced in that comparison. These findings and baseline counts do not make
the scanner gate green. Final scan source SHA256s match the final six Rust files.
Receipts are `/tmp/cass-538-ubs-baseline.log`, `/tmp/cass-538-ubs-final.log`,
`/tmp/cass-538-ubs-final-source-sha256.txt`, and
`/tmp/cass-538-ubs-final-comparison.json`; review notes are in
`/tmp/cass-538-ubs-baseline-comparison.md`.

## Follow-up from `a93afc1` (2026-10-30)

This follow-up starts from `a93afc1e85f069f606a64eea4ce810a30a0a3bc8` and
preserves the corrections and historical evidence above. It addresses additional
recovery and diagnostic defects found by reviewing the actual watch path. It
does **not** establish that the original macOS `lstat` stall is resolved.

### Additional correctness changes

| Remaining defect | Correction and invariant |
| --- | --- |
| An otherwise successful callback consumed its event even when lexical publication or a transient NoMem failure retained replay debt. Nothing guaranteed a later event. | Return explicit retry roots with the reindex outcome and retain them in the existing cooldown queue, including scans held entirely by NoMem backoff. Permanent exclusions and quarantines alone do not create endless rescans. |
| A process restart lost the in-memory pending queue after durable replay debt had been recorded. | Seed recovery once from durable debt, restricted to the currently configured connector, source ID, and intersecting root. Removed sources are not resurrected; malformed debt is reported rather than silently discarded. |
| Cumulative `current`/`total` counters were being used to infer watch idleness. Failed attempts can leave `current < total` after recovery; completed counts can also precede blocked I/O. | An RAII scope marks only the owner's actual event/cooldown wait. The marker clears before callback work and on cancellation, disconnect, or unwind. The watchdog pauses during that real wait without changing work or activity counters, and re-arms for active phase-zero I/O even when `current == total`. |
| The watchdog synchronously inspected the same filesystem it was diagnosing. A blocked diagnostic `stat` or `open` could delay the abort itself. | At most one separate filesystem probe is pending per watchdog. The watchdog only polls its receiver and never joins it. Every event gets a fresh in-memory `raw_mirror_io` snapshot. `filesystem_probe.status: "pending"` means unfinished diagnostics, not absent assets. Completed/buffered probes are retired on real progress or idle transitions; an unresolved worker is retained. |
| Diagnostic reads trusted a prior metadata length, and the abort checkpoint checked database existence before starting its terminal deadline. | Bound actual diagnostic reads to 64 KiB plus one overflow-detection byte. Put the database existence probe inside the existing bounded checkpoint operation, retaining ownership and the existing deadline. A missing database remains a no-op. |
| Several real capture/publication filesystem operations were missing from in-flight diagnostics. Instrumenting mandatory post-publication steps must preserve their non-cancellable cleanup/durability behavior. | Observe metadata/open/create, writes, link/rename publication, permissions, sync, and cleanup. Mandatory post-publication operations are observed without introducing cancellation boundaries. Activity advances after an operation returns, never while a syscall or lock acquisition is blocked. |
| `Path::exists()` followed a dangling canonical-manifest symlink, so the refusal happened after staging and leaked a temporary file. | Use observed `symlink_metadata` and refuse the unsafe existing path before staging. The current regression fails with the specific staging-leak assertion on the pinned starting commit and passes on the corrected source. |

The normal stall thresholds, terminal exit code 70, mutation and index leases,
generation fence, conservative mirror accounting, source checks, and durable
watermark/publication rules are preserved. These changes do not declare deferred
work complete to quiet the watchdog.

### Full application regressions

`tests/watch_e2e.rs` now retains distinct stdout/stderr files for every watcher
process, so a restart cannot truncate its predecessor's stall evidence and a
full pipe cannot block the watcher. Its command helper accepts the existing
`CARGO_BIN_EXE_cass` binary override through `util::cass_bin`.

The existing live no-op/update/restart regression additionally preserves raw
blob contents and mtimes, alongside manifests, source caches, staging metadata,
and the generation fence. It exercises repeated real watch discovery beyond its
configured abort window, successive appends, process interruption/restart, and
preservation of older searchable results without duplicate hits.

The additional live stall regression first waits for a resident native watcher.
It then holds a real raw-mirror mutation lease, writes a real new source, and
places an unpaired FIFO at a legacy diagnostic-only checkpoint pathname. It
requires the exact `acquire_mutation_lock` path and increasing elapsed time,
unchanged watermark/generation while work is incomplete, old results remaining
searchable, competing-index exit 7, and watchdog exit 70 with the optional probe
still pending. It releases the lease and restarts without rewriting the pending
source, then checks recovered results and another append. Literal `--data-dir`
path spelling is retained for macOS `/var` versus `/private/var` aliases.

The storage-backed library regression also covers failure to write replay debt
before canonical mutation, deferred publication, loss of the pending queue,
storage reopen, recovery without a new source event, and preservation of
unrelated results. Scope and permanent-exclusion tests constrain replay seeding.
These application-level tests were reviewed and formatted, but **were not
executed in this environment** because the application did not compile within
the available resources.

On a provisioned host, run the full gate and the live selection:

```sh
RUST_MIN_STACK=134217728 \
  cargo test --locked --test watch_e2e gh538_live_watch -- --nocapture --test-threads=1

GATE_BUILD_JOBS=1 scripts/gate.sh --local \
  --lib-filter 'gh538 raw_mirror watch_spool watch_idle_wait watch_reindex watch_lexical reindex_paths_carries_only_new_packet_semantic_delta_messages watch_conversation_sort stall_diagnostics_tests' \
  --integration 'watch_e2e:gh538,cli_index:gh439_slow_post_publish_fts_repair_is_not_aborted_while_it_heartbeats gh439_parked_post_publish_fts_repair_still_aborts_with_the_index_stalled_envelope'
```

### Executed supplemental regressions

The source-faithful runner is now committed at
[`scripts/tests/gh538/`](../../scripts/tests/gh538/README.md). It extracts selected
production code, records input/range/output hashes, verifies the compiled test
inventory and exact executed binary, and builds with `--locked` dependency
subsets checked against the repository lockfile. No generated copy of production
Rust is committed. Baseline and candidate have distinct crate identities, and
the negative control always uses the pinned `a93afc1` object, never moving `HEAD`.

```sh
python3 scripts/tests/gh538/run.py --output-dir /tmp/cass-gh538-components
```

Use `--offline` with a populated cache. The README documents the pinned-toolchain
directory override, reusable target directories, extraction-only verification,
recorded environment, and all exclusions. Every invocation preserves its own
timestamped report and logs; a compile error or a missing/skipped test cannot
silently satisfy the runner.

The final frozen-source run on Linux tmpfs passed **19/19 mirror tests and 32/32
watchdog/diagnostic/idle tests**. The separate baseline invocation ran one
regression and reproduced exactly the expected staging-leak assertion; this is
a successful negative control, not a passing baseline test. The mirror suite
retains the 20,816-file / 9,523-manifest / 11 GiB logical fixture and twelve
append/no-op and refusal cycles. Sparse bytes and tmpfs are not physical 11 GiB
disk or APFS performance measurements.

The FIFO subprocess probe first proved that synchronous diagnostic collection
really blocks, then killed and reaped that owned negative-control child. With
the asynchronous production watchdog, the abort decision occurred at **3,009
ms** and process exit **70** was observed at **3,060 ms**. The fresh raw-mirror
snapshot identified the actual blocked FIFO operation/path with **3,014 ms**
elapsed, while the diagnostic probe remained pending. Work and activity counters
were unchanged. This used the supported three-second abort setting without
weakening the production decision logic.

An earlier run of the packaged suite failed twelve mirror tests when the overlay
disk filled. That run remains a failure. The subsequent successful runs used
real tmpfs fixtures and separate evidence directories; they do not erase the
disk-exhaustion result. The final source, commands, counts, hashes, exclusions,
and static-analysis summary are recorded in
[`issue-538-followup-validation.json`](issue-538-followup-validation.json).

This runner does **not** execute provider ingestion, canonical SQL, lexical
publication/search, durable replay recovery, SQL cancellation/WAL checkpointing,
or a complete CASS watcher. It projects the progress carrier only to remove the
SQL cancellation relay and unused statistics fields, with that projection
recorded explicitly. It neither substitutes SQLite nor calls the component
result a full application pass.

### Full validation remains red

Two direct attempts to build the live-watch test, with one Cargo job and the
pinned dependencies/toolchain, were killed while compiling `asupersync 0.5.0`
under the shared 8 GiB cgroup limit. The second used the installed Cranelift
backend for that dependency and still received SIGKILL. Neither reached the
application test executable; no dependency source or version was substituted.

The subsequent full gate used a wrapper that preserves compiler arguments but
waits for headroom and refuses compilation below 750 MB available disk. It
refused `asupersync` at approximately 731 MB free, and later phases had still less
space. `fmt` passed; clippy, library build/tests, both integrations, binary,
and goldens returned 101 without positive terminal test counts. The checkout
also changed during that attempt, so source stability failed. This is a red
attempt against an earlier snapshot, not a full gate for the final source.
Final frozen-source formatting and whitespace checks passed separately.

The gate's UBS invocation lacked the parser rule-pack and is not complete
static-analysis evidence. A separate identical full-five-file comparison used
pinned UBS v5.4.4, real ast-grep 0.45.3, the unchanged exclusions, and
`--ci --fail-on-warning`. Both baseline and final candidate remain **strict exit
1**:

| Follow-up snapshot | Critical | Warnings | Info | Strict exit |
| --- | ---: | ---: | ---: | ---: |
| Baseline `a93afc1` | 76 | 16,826 | 8,441 | 1 |
| Final follow-up source | 78 | 17,010 | 8,473 | 1 |

All six real parser invocations in each scan completed with exit 0. The mapped
warning delta is 202 added and 18 removed; 197 additions are in tests. The
validation receipt records source identities and parser invocation status.
Added critical findings are deliberate live-test
failure paths. Reviewed production findings concern owned retry-root copies and
bounded diagnostic JSON object updates; that review does not turn a red scanner
gate green. No rules, suppressions, thresholds, or required gates were weakened.

### Native macOS qualification still required

No native macOS host or the reporter's archive was available. The executed Linux
FIFO probe establishes the selected diagnostic mechanism; the held-lock live
watcher regression remains unexecuted. Neither establishes the original kernel
`lstat` cause or its disappearance on APFS.

Run the exact candidate binary with the reporter's original source roots,
retention/cap settings, preflight settings, and watch interval. Keep the existing
stall watchdog enabled and use the supported bounded trace command:

```sh
CASS_TRACE_FILTER=warn,coding_agent_search::raw_mirror=trace \
  cass --trace-file cass-watch-538.jsonl index --watch --watch-interval 30 --json
```

Retain the version/commit, complete command and relevant `CASS_*` settings,
stdout, stderr, exit code, timestamps, and trace artifact. If it stalls, capture
the full structured stall event as well as the compact `index-stalled` envelope.
The event's `diagnostics.raw_mirror_io` supplies the operation/path/elapsed time;
`diagnostics.filesystem_probe.status` states whether optional filesystem context
finished. A pending probe is not evidence that the files are absent. A process
sample taken during the same interval can then be correlated with the named
operation rather than guessing a pathname from the old stripped stack.

Qualification should include repeated quiet scans spanning several of the
original approximately 300-second windows, new sources and successive appends,
interruption/restart, unchanged archive captures during no-ops, and preservation
of known old and new searchable results. Record durations and counts rather than
assuming that one successful startup run qualifies a resident watcher.

Two narrower boundaries also remain: this change fixes the demonstrated dangling
symlink staging leak, not every later write/fsync/link/rename error-cleanup path;
and replay recovery requires debt to have been established. A historical local
import with preserved old mtime, a lost event, and termination before discovery
has recorded debt is not covered by these tests. The original issue must not be
called resolved on the strength of the component receipts alone.
