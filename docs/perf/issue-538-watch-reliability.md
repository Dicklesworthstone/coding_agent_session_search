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
