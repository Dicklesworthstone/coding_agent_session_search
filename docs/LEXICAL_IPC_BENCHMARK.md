# Qualifying the guarded lexical IPC path

See `LEXICAL_IPC.md` for the explicit foreground-owner deployment and integrity
boundary. This harness measures that alternative; it does not claim that
ordinary standalone `cass search` has stopped hashing segments.

```sh
cargo build --locked --release --bin cass --bin cass-query
python3 -S scripts/benchmark_lexical_ipc.py \
  --cass target/release/cass --client target/release/cass-query \
  --index /absolute/path/to/published-index --query performance --rounds 10 \
  --output /absolute/path/to/new-evidence.json
```

Use an existing index and a query with at least one hit. The output path must
not already exist. No index files are written or caches flushed. Linux and
procfs are required. Omit `--client` only to measure the separately labeled
Python frontend; a missing explicitly requested native client is an error,
not a silent fallback.

## Three distinct workloads

`strict_fresh_process` starts a new native `cass serve` for each single query.
It records response latency, complete process wall time, and complete native
child CPU after EOF and a clean exit. This is engine-oriented one-shot lexical
admission, not all ordinary `cass search` CLI readiness/maintenance overhead.

`retained_native` opens one native process. Initial admission is separate from
its subsequent query samples. Complete-group wall time and native CPU include
startup and teardown; no per-query native CPU number is fabricated while that
process remains alive.

`fresh_ipc_clients` runs one explicit owner but a fresh client process for every
query. It separates the first strict admission, retained-reader client
requests, a forced full-verification control, and reuse after that control.
Each sample's CPU is **client-only**. Complete-group CPU is collected after
reaping every client and the owner, which itself reaps its native children;
Linux `RUSAGE_CHILDREN` then includes those descendants. This prevents an
apparently cheap frontend from hiding the owner's work. The complete group
includes initial admission, full-verification control, and teardown, so it is
not a steady-state per-query CPU estimate.

Phases run in the recorded order: fresh native, retained native, then IPC.
Filesystem cache state is not reset or certified. Fresh process does not mean
cold storage. Do not turn these measurements into a cold-disk or large-archive
speedup claim without a representative controlled experiment. The harness
reports raw samples rather than selecting a favorable speedup statistic.

## Evidence and refusal rules

The report fingerprints both binaries, the owner source, and the actual Quill
`MANIFEST`; records host/query/rounds and an optional CI revision; and checks
complete hit objects, identities, scores and ordering across every phase.
Admission/reuse transitions and the full-verify control are asserted, not just
labeled. A retained inode guard covers the index throughout qualification.

Empty workloads, request errors, changed index identities or executable bytes,
malformed or excessive output, timeouts, score/order mismatches, policy
downgrades, and unexpected exit/cleanup behavior refuse publication. An
existing evidence file is never replaced. Process stdout/stderr capture is
bounded. Failed runs must not be presented as performance evidence.

## Validation

```sh
python3 -S -m unittest discover -s scripts -p 'test_*lexical_ipc.py' -v
cargo test --locked --release --bin cass-query
```

The combined Python suite contains 26 tests: 20 exercise the real guard/socket
implementation with fake strict workers, and six exercise benchmark success
and refusal behavior. Those are transport/control-flow tests, not CASS timings.
The native client has three separate Rust test definitions.

`.github/workflows/lexical-ipc.yml` runs the Python suite, then builds and tests
the native client and strict engine contract. It generates a disposable real
CASS index and runs all three workload phases with the actual `cass` and
`cass-query` binaries. A committed workflow or queued run is not a successful
native qualification. Its small 4,096-document fixture is an integration smoke
workload, not the maintainer's 1.4 GB corpus.
