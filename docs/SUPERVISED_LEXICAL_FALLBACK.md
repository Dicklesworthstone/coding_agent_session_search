# Supervised lexical recovery after a Quill fuel refusal

`cass serve` can explicitly opt in to the existing FrankenSQLite lexical lane
when its loaded Quill reader refuses a query's work budget:

```sh
cass serve --stdio \
  --data-dir /absolute/path/to/cass-data \
  --db /absolute/path/to/agent_search.db \
  --lexical-fallback --request-timeout-ms 10000
```

Example newline-delimited requests:

```json
{"id":1,"op":"search","query":"git fixture","limit":1}
{"id":2,"op":"search","query":"test isolation","limit":1}
{"id":3,"op":"status"}
```

The same startup option applies when using `--mcp`. The request uses the same
archive path selected at startup; query text cannot choose another archive.
`--db` alone continues to authorize canonical follow-up, not automatic search
fallback. Startup and status do not open the fallback archive.

## Scope and admission

This is a **service fallback**, not a new backend selector for ordinary
`cass search`, and not a dependency or fuel-budget increase. Main's existing
Quill 0.4.0 pin and the earlier GH518 linear-fuel regression are separate work.
A missing/unreadable lexical index still fails admission: this feature handles
a fuel refusal from a successfully loaded reader, including one propagated
from its internal overfetch or federated search.

A successful empty Quill result remains authoritative. Corruption, invariant,
parser and cancellation failures are not converted to an empty result or an
archive search. The bridge preserves the concrete Quill error, and a typed
non-fuel failure overrides even misleading outer diagnostic text. The classifier
retains narrow compatibility with historical, already-flattened fuel diagnostics.

Before opening SQLite, the fallback checks the observed size of the database
and its `-wal` file. Their checked sum must not exceed **64 MiB**. Missing WAL
is valid; other metadata errors, non-files and overflow are refusals. This is
a conservative admission observation, **not** a fixed-snapshot size limit:
concurrent changes can occur after the metadata check.

The service's existing process-level watchdog is the total execution bound.
It starts with the first request-frame byte and remains in force through the
Quill attempt, SQLite fallback, connection teardown, response serialization
and flush. Fallback does not restart the timer, raise fuel or create a detached
query thread. Expiry terminates the worker with exit code 124. The existing
sampled RSS supervisor and reader-admission lease also remain active; sampling
is not a peak-RSS or kernel-allocation guarantee.

The SQLite client is fresh, strict-read-only, and has no lexical reader,
semantic context, model, warm worker or response cache. Its reader is dropped
before fallback completion is counted. Nothing automatically rebuilds the
index, replaces the canonical archive or retries Quill with additional fuel.

## Result provenance and semantics

Every successful service lexical response identifies its serving backend:

- `backend: "quill"`, `degraded: false`: the pinned primary reader served it.
- `backend: "frankensqlite"`, `degraded: true`: explicit archive recovery served it.

A recovered response carries the original fuel refusal in `fallback`, the
observed archive size, and the changed snapshot policy. `reader_reused` is
false and `result_reader_epoch` is null because a fresh SQLite reader, not the
retained Quill reader, served the results. The existing `snapshot` object is
**worker status**: it may still show that a primary Quill reader is loaded.
It is not a certificate that fallback results came from that generation.

The supported SQLite lane can use FTS5 or the source-table scan, depending on
query grammar and FTS usability. Its exact choice is not exposed by
`SearchClient`, so `fallback.sqlite_lane` is null and both possible lanes are
named. A source scan is never reported as proven FTS5 execution.

This is explicitly degraded retrieval: SQLite scoring, prefix behavior and
freshness can differ from Quill. In particular, successful recovery is not a
claim of Quill BM25/rank/prefix equivalence or stable pagination across writes.
The original query, structured filters, offset, limit and normal result reducer
are passed to the existing SQLite search path without textual rewriting.
Refinement preserves the retrieval backend and degradation metadata alongside
its separate reranking result. Semantic-to-lexical degradation that already
uses service lexical search preserves those fields as well.

For an ordinary lexical request, if fallback fails, the error retains the
original fuel refusal and adds the
fallback failure plus the manual repair command. It does not report partial
results as a successful search. Oversized archives must be maintained outside
the interactive request:

```sh
cass --db /absolute/path/to/agent_search.db \
  index --full --json --data-dir /absolute/path/to/cass-data
```

Select matching archive/index coordinates and preserve a consistent backup
before maintenance. This command is not a verified cure for every archive.

## Recovery inside hybrid retrieval

With `--semantic-embedder` and the required explicit archive/data directory,
`--lexical-fallback` also applies to the lexical leg of `semantic_search` in
`hybrid` mode. It is no longer bypassed merely because semantic retrieval
succeeded. The same `Session` route serves JSON-lines, MCP, and one-shot hybrid
requests; no second recovery implementation or deadline is introduced.

```json
{"id":1,"op":"semantic_search","mode":"hybrid","query":"git fixture","limit":2}
```

When Quill exhausts fuel, eligible recovery reads the same query and exact
structured filters at candidate offset zero with the existing fusion window.
The temporary archive reader is dropped before its candidates are fused with
retained semantic candidates. Pagination is applied after fusion. Semantic
asset/archive change checks still run after the lexical leg, so a detected
change prevents stale semantic coordinates from being published as fused hits.
Those checks do not establish an immutable cross-backend or cross-page snapshot.

A completed two-leg result retains `realized_mode: "hybrid"`. Its response now
identifies `lexical_backend` as `quill` or `frankensqlite`; recovered results
carry `lexical_fallback`, set `degraded: true`, and leave
`lexical_result_reader_epoch` null. The latter records a Quill epoch only when
that pinned reader actually supplied the lexical result. The recovery receipt
retains the original failure and explicitly disclaims equivalent Quill ranking.

Without recovery consent, or when eligible recovery fails, the existing hybrid
policy still returns available semantic results with `realized_mode: "semantic"`
and `lexical_degrade_reason`. This also remains the behavior for non-fuel lexical
errors: they cannot authorize an archive search, but are not newly made fatal to
a successful semantic leg. An empty successful Quill result does not trigger
recovery. Semantic-only requests never load a lexical reader for this feature.

If semantic retrieval itself is unavailable, the existing lexical-only response
continues to use `backend` and `fallback`, as described above, alongside its
semantic failure reason; it is not a successfully fused two-leg response.

## Regression coverage and verification status

`tests/quill_fuel_fallback.rs` defines real-binary tests for both reported
queries with forced one-unit Quill fuel, consent isolation, unavailable
recovery through oversized/corrupt archives, preservation of authoritative
empty results, and a 206-segment index whose actual live segment count is
asserted before running both queries under 80,000 units. Unit tests cover the
DB-plus-WAL size boundary and non-resource error routing.

`tests/search_service_hybrid_recovery.rs` adds six real-process tests with real
hash FSVI, Quill, and canonical archive fixtures. They cover recovery consent,
reader reuse/unload under one admission slot, authoritative empty lexical
results, semantic-only isolation, agent/workspace/source/time scoping,
pagination after fusion, and missing-index refusal without archive recovery.
Each compares the complete tiny data-directory file inventory and bytes.
These tests are authored but have not been executed in this environment.

The fixtures use real Quill and FrankenSQLite data. Fuel is configured on each
child process, not by changing environment variables in a parallel test runner.
Every child has an external teardown bound in addition to the service deadline.

Run the repository's normal RCH gate with the new integration target and the
existing service target. Check `scripts/gate.sh --help` for the gate's selection
syntax and fleet setup. Direct native test selections are:

```sh
RUST_MIN_STACK=134217728 cargo test --locked --test quill_fuel_fallback
RUST_MIN_STACK=134217728 cargo test --locked --test search_service
RUST_MIN_STACK=134217728 cargo test --locked --test search_service_hybrid_recovery
```

At implementation time in this session, Cargo, rustc, rustfmt, RCH and UBS were
unavailable. The new Rust tests, compilation, formatting, Clippy and native
latency/work benchmarks therefore remain **unexecuted**. Source-blob identity
and whitespace checks are not substitutes for those gates. No speedup or
successful runtime fallback measurement is claimed by these test definitions.
