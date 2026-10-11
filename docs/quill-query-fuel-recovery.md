# Quill query-fuel failures: investigation and bounded measurements

Issue: [#518](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/518).

**Status:** investigation and benchmark tooling, not a landed runtime fix.
The driver tests described below do not build fragmented Quill indexes, execute
CASS, or establish a latency improvement. Keep #518 open until those checks pass.

## What the inspected code establishes

The initial CASS revision examined was
`fb51773cac0ae07d1631a01fb78897c71d639713`. Relevant sources:

- [`src/search/quill_bridge.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/quill_bridge.rs)
  keeps the normal Quill budget at ten million units unless a positive
  `CASS_QUILL_QUERY_FUEL_BUDGET` override is supplied. `search_paginated` wraps
  engine errors as strings; `is_query_fuel_exhausted` recognizes the error chain
  by its message. Work units are not a wall-clock deadline.
- [`src/search/query.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/query.rs)
  executes `search_tantivy` with a **Quill** reader and native CASS query parser.
  `SearchClient::search` propagates failures from the primary call, its
  overfetch retry, and the federated lane with `?`. The SQLite lane below those
  branches is reached when no lexical reader is available, not when a live
  Quill reader exhausts fuel. A successful empty primary result is deliberately
  authoritative; it must not resurrect stale SQLite matches.
- The SQLite lane is not automatically a cheap indexed substitute.
  `search_sqlite_fts5` falls back to `search_sqlite_message_scan` for unsupported
  grammar, missing/unusable FTS, integrity failures, and several execution or
  hydration failures. The source scan uses keyset pages inside one read
  snapshot and retains a bounded top-k, but its outer loop still traverses the
  eligible archive. Page size is not a request-wide row, byte, or time budget.
- [`src/search/tantivy.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/tantivy.rs)
  retains compatibility names while using Quill for ingestion and execution.
  [`tests/logging.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/tests/logging.rs)
  still expects `backend="tantivy"`. Neither that label nor a `search_start`
  event proves that an independent Tantivy engine served the result.

There is important upstream history in
[Frankensearch #41](https://github.com/Dicklesworthstone/frankensearch/issues/41).
The [September 20 correction](https://github.com/Dicklesworthstone/frankensearch/issues/41#issuecomment-5752454821)
reports that cached snapshot statistics had been charged again per segment,
although those dictionary reads were not repeated. It also describes sharing
statistics with range/set/glob lowering and changing the matching work ceiling.
Consequently, the #518 dictionary counter alone is **not** evidence of ten
million physical dictionary probes, nor proof that the same old accounting bug
remains in the reporter's binary. Verify the actual engine source/version used
by that binary before choosing an upstream patch or dependency update.

Fragmentation remains relevant independently of the meter: reopened writers can
leave sparse document-ID leases. The
[September 11 upstream experiment](https://github.com/Dicklesworthstone/frankensearch/issues/41)
reported substantial size amplification from forcing concat over sparse leases.
Do not remove merge hole guards, force large query-time merges, or increase the
global fuel budget as a substitute for a measured correction.

## Recovery without deleting the canonical archive

Use the **same database and data directory as the failing command**. Set these
variables to the actual paths; do not paste the placeholders unchanged:

```sh
DB=/absolute/path/to/agent_search.db
DATA_DIR=/absolute/path/to/cass-data
cass --version
cass --db "$DB" index --full --json --data-dir "$DATA_DIR"
cass --db "$DB" search 'git fixture' --limit 1 --json --data-dir "$DATA_DIR"
cass --db "$DB" search 'test isolation' --limit 1 --json --data-dir "$DATA_DIR"
```

`cass index --full --json` with matching `--db`/`--data-dir` is the recovery
command already emitted by
[`inline_repair.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/inline_repair.rs).
It is a manual, potentially expensive rebuild, **not** a verified cure for the
reporter's archive. Preserve a consistent backup first, arrange enough free disk
space, and stop competing indexing/watch activity during the maintenance window.
Do not delete `agent_search.db`, its WAL, source session files, or the last usable
index to work around a query refusal. A filesystem copy of an actively written
SQLite database without its consistent WAL state is not a valid backup.

Preserve the original failure, executable identity, engine versions, and actual
live segment count before rebuilding. A count of segment files on disk is not
necessarily a live segment count: retired files can still be present. Compare
result identities, filters, and live document counts afterward, not just exit
status. Capture any rebuild error separately; repeatedly retrying the original
query with more fuel is not recovery.

## Bounded command-level measurements

`scripts/benchmark_quill_query_fuel.py` requires Python 3.11+ and POSIX process
groups (Linux/macOS). Run it against a **disposable, consistent copy** of the
archive and index. CASS search itself can perform maintenance; this script does
not promise read-only CASS behavior and never invokes a rebuild.

```sh
python3 scripts/benchmark_quill_query_fuel.py \
  --cass /absolute/path/to/cass \
  --db /absolute/path/to/disposable-copy/agent_search.db \
  --data-dir /absolute/path/to/disposable-copy \
  --disposable-copy --reported-segment-count 206 \
  --label 'baseline: record source revision and corpus identifier here' \
  --repetitions 5 --timeout-seconds 10 \
  --output /absolute/path/to/new-gh518-baseline.json
```

The default queries are `git fixture` and `test isolation`, explicitly in lexical
mode with limit one. `--reported-segment-count` is operator-supplied provenance,
not a count the script verifies. `--query` can be repeated for additional cases.
Each invocation gets a fresh process; the driver does **not** flush OS caches or
measure a warm in-process searcher. Latency includes CLI startup, index open,
search, hydration, shutdown, and observation overhead. Query order alternates.

The default external deadline is ten seconds per invocation, with a combined
256 KiB stdout/stderr capture cap. A deadline or output overflow kills/reaps the
private process group. This contains ordinary children in that group; it is not
proof of engine-internal cancellation and does not cancel unrelated daemons or
processes that deliberately detach into another session. No shell is involved.
Limits on repetitions, query count, and timeout prevent an accidentally
unbounded suite. Use a disposable local setup without external query workers.

The receipt includes executable SHA-256, requested budget override, per-query
outcomes and latency percentiles, result counts, distinct fuel-refusal counters,
and observed `search_start` labels. It does **not** persist returned message
content or raw stderr. It does retain the query and explicitly supplied paths,
so review the receipt before sharing it publicly. Output files are never
silently overwritten.

Interpretation rules:

- Failed, timed-out, or malformed-output samples are counted separately, not
  included as artificially fast successful samples. Exit status is nonzero if
  any sample fails. A successful empty result still needs a correctness oracle.
- Missing work counters are `null`, **not zero**. Current CLI success output does
  not supply a successful-query work receipt. Refusal counters measure admitted
  work, not measured physical probes. Repeated error context is deduplicated.
- `serving_backend` remains `null`: attempted backend labels alone are
  insufficient evidence. In particular, this driver does not relabel
  `tantivy` as a demonstrated independent backend or claim a successful fallback.

An explicit `--fuel-budget N` runs every sample at that single positive budget.
The inherited environment override is removed when this option is absent.
There is no adaptive increase and no automatic retry after a refusal. For a
before/after comparison, run separately with each executable on equivalent
snapshots and retain both receipts. A timeout is a censored observation, not an
engine latency percentile. Actual successful-query work measurements require
an engine-facing instrumented benchmark, separate from this CLI driver.

Driver validation:

```sh
python3 -m unittest discover -s scripts/tests \
  -p 'test_benchmark_quill_query_fuel.py' -v
```

The tests use small child processes, a fake CLI, and the reported **log** shape.
They test process-group cleanup (including descendants holding pipes), output
limits, closed-pipe deadlines, spawn failures, JSON validation, percentile
accounting, environment isolation, and non-overwritten receipts. They are not
Rust compilation, Quill correctness tests, or corpus-scale benchmarks.

## Runtime acceptance work still required

A runtime correction must cover the primary, overfetch-retry, and federated
paths under a shared request budget. Keep a legitimate zero-hit result
successful. Catch only an appropriate resource refusal; do not hide corrupt
indexes, parser/invariant errors, or storage failures behind empty results.
Use a supported fallback only when its semantics and **total execution cost**
are bounded. Preserve title/content semantics, metadata filters, provenance,
deduplication, pagination, and coherent archive reads. A row-page cap or SQL
`LIMIT` after ranking is not an instruction budget or a deadline.

Report the actual serving backend and degradation reason per request, including
cache-hit cases, and never store fallback results under a misleading Quill-only
generation. Failed or unavailable fallback must retain the original refusal and
an actionable rebuild command with the effective archive paths. Optional counts
and repeated retrievals must not silently create fresh unlimited retry budgets.

Native regression fixtures must assert their **actual** segment counts (including
206), keep fragmentation from being merged away by the test setup, and exercise
both continuous commits and reopened sparse writer sessions. Include both
reported queries, no-match controls, supported/unsupported fallback queries,
forced low-fuel refusal, unavailable/corrupt fallback, filters, pagination,
deletions, and post-rebuild result identity. Benchmark small and 206-segment
fixtures with the same logical documents, separate opening from warm execution,
and record work counters alongside latency. No such native results are claimed
by this change.
