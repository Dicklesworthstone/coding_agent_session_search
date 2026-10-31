# Supplemental GH538 component regression runner

This runs the actual selected production Rust against real filesystem operations
when the full application build is unavailable. It **does not pass the full Rust
gate, run the live CASS watcher, validate canonical SQL/search recovery, or qualify
the reporter's original macOS stall**. See the investigation record at
[`docs/perf/issue-538-watch-reliability.md`](../../../docs/perf/issue-538-watch-reliability.md)
for those separate requirements and the supported reporter trace command.

## Run

From the repository root, with Python 3.11+, the toolchain pinned in
`rust-toolchain.toml`, and a POSIX host with `mkfifo`:

```sh
python3 scripts/tests/gh538/run.py --output-dir /tmp/cass-gh538-components
```

Add `--offline` when all locked dependencies are cached. `--toolchain-bin DIR`
accepts an installed directory containing the pinned `cargo` and `rustc` binaries
when rustup shims are unavailable. `CARGO_HOME` is honored. Builds use one Cargo job,
debug information disabled, and incremental compilation disabled. `RUSTFLAGS`
defaults to `-Z threads=1`; any supplied flags and compiler wrappers are recorded.
The runner pins both Cargo compiler overrides to the queried `rustc` executable
and records hashes of applicable Cargo configuration files, so an inherited
`RUSTC` setting cannot silently select a different compiler.

The baseline object `a93afc1e85f069f606a64eea4ce810a30a0a3bc8` must exist locally.
For a shallow clone missing it, fetch that exact object from the trusted repository
before running. The negative control never reads moving `HEAD` as its baseline.

The command creates a unique `runs/<UTC timestamp>-<PID>/` evidence directory,
then extracts three independent crates there,
builds each with `--locked`, obtains the test executable from Cargo's JSON artifact
record, checks its complete test inventory, and runs these selections serially:

| Component | Selection and required outcome |
| --- | --- |
| `mirror-candidate` | All 19 selected production filesystem/policy regressions pass. |
| `mirror-baseline` | One current dangling-manifest-symlink regression runs against production code from the pinned baseline and fails with the specific staging-leak assertion. Compilation errors, other failures, or an unexpected pass fail the runner. |
| `watchdog` | All 28 selected production watchdog/diagnostic tests, three production idle-wait tests, and the separate real-FIFO subprocess probe pass. |

Counts are verified against the extracted selection and the compiled libtest
inventory, so a renamed, skipped, or silently absent test cannot yield a green
report. New production tests within a selected region are included automatically;
the recorded inventory gives the authoritative count for that source version.

Each run's `report.json` records commands, exit codes, durations, toolchain versions, build
flags, executable hashes, source verification, test counts and exclusions. The
`logs/` directory retains build output, inventories, test output, and the FIFO
watchdog's real warning/abort payloads. The top-level `report.json` identifies the
latest run; previous run reports, logs, and generated source remain intact.
Every generated crate has a `sources.json`
containing whole-input hashes, byte ranges and hashes for selected source, support
file hashes, exact output hashes, and explicit projection details. Source and
generated-output verification runs before and after each test. Existing Cargo
targets can be reused with `--mirror-target-dir DIR` and
`--watchdog-target-dir DIR`; Cargo still checks source/dependency identities and
the runner records the specific executable it actually executes.

To extract or check evidence without building:

```sh
python3 scripts/tests/gh538/extract.py --output-dir /tmp/cass-gh538-components
python3 scripts/tests/gh538/extract.py --output-dir /tmp/cass-gh538-components --verify
```

## Source and dependency fidelity

The repository stores the extractor, runner, dependency locks, and two clearly
identified FIFO support files. It stores **no generated copy of production Rust**.
The extractor fails if its named source boundaries disappear. Changes to source,
support, locks, extraction logic, or generated files invalidate `--verify`.

`locks/*.Cargo.lock` pin every transitive dependency. Extraction verifies each
non-harness package's name, version, source and checksum against the repository's
`Cargo.lock`; subset feature selections can legitimately omit dependency edges.
The generated manifests pin the same direct dependency versions. Baseline and
candidate mirror crates have distinct package identities, and the baseline lock
differs only in the harness package name. Cargo cannot silently refresh these
locks because every build uses `--locked`.

The mirror extraction omits the SQL-dependent prune entry point and pinned-tag
queries, automatic retention, and unrelated tests. It retains the selected
filesystem, admission, publication, inventory, verification, and observation code
verbatim; there is no fake filesystem or fake database.

The watchdog extraction preserves the actual watchdog thresholds and decisions,
diagnostic worker, raw-mirror observer, atomic progress methods, JSON snapshots,
event wait and RAII idle marker. Its only behavior projection removes
`IndexingProgress.storage_cancellation` and the corresponding SQL cancellation
relay; atomic stop requests and checks remain exact. Its small `IndexingStats`
carrier contains only the two verbatim fields used by snapshots. It does not test
the SQL WAL checkpoint or `abort_after_index_stall_if_requested` wrapper.

The separate FIFO probe first proves that exact synchronous diagnostic collection
blocks on a real unpaired FIFO, then kills and reaps that owned child. A second
child runs the exact asynchronous watchdog while the exact raw-mirror observer
tracks another real FIFO open. It requires live in-flight path/operation evidence,
unchanged work/activity counters, the supported three-second abort setting, a
real `index-stalled` envelope, and process exit 70. The production I/O sampler is
restored where unit-test configuration otherwise substitutes `None`. No watchdog
deadline is weakened. This qualifies the selected blocked-I/O mechanism on the
executing host; it does not reproduce macOS `lstat` behavior or an entire watcher.

The full live regressions remain in `tests/watch_e2e.rs`. On a host able to build
the whole application, run the normal Rust gate and, separately:

```sh
cargo test --locked --test watch_e2e gh538_live_watch -- --nocapture --test-threads=1
```

The component runner never marks those application-level requirements complete.

## Watch source stability and receipt bounds

The existing source-ledger matcher harness also has an opt-in GH538 component
mode. It copies the production filesystem matcher, the complete
`watch_source_stability.rs` helper and its eight tests, and the production
`ReceiptBuffer` implementation and its two size-limit tests:

```sh
python3 scripts/test_source_ledger_matcher.py --watch-components --offline \
  --output-dir /tmp/cass-gh538-watch-sources
```

Add `--watch-scan` to include the exact `WatchSourceLedger::scan` method and the
two synthetic connector boundary tests. This option implies
`--watch-components`. Those tests mutate real source files after delivering a
prefix, withhold the completion event as Codex and Claude do, and require a retry
signal both at the next source boundary and at the end of the scan. They then
scan the settled source and require its complete bytes and a completion receipt.
The SQL methods `begin_publication` and `publish_batch` are excluded completely;
the harness supplies no simulated database.

Both modes verify the cached `franken_agent_detection` checkout against the
exact revision in `Cargo.lock`. `--fad-checkout DIR` can select that checkout
explicitly. The smaller mode extracts its actual source metadata type and
methods; scan mode also extracts the actual normalized/provenance types,
`Connector` trait, scan context and source hooks. The CASS observation, key and
workspace-policy helpers used in scan mode are copied verbatim.

The generated report lists each adapter: a minimal `ScanRoot` carrier, fixture
dispatch to the production dependency-policy declarations, and, for scan mode,
an absent-progress adapter and a deliberately inactive source-filter adapter.
Progress methods panic if unexpectedly called. The scan fixture uses an
explicit supplemental producer-contract string and no path/workspace
exclusions. Its two `serial_test` annotations and import are the only changes to
the selected boundary tests; the executable always runs with one test thread.
This does **not** validate active-writer filtering, cancellation, the complete
factory registry, aggregate receipt-map bounds, receipt/debt transactions,
native notifications, or end-to-end ingestion and searchability.

The small dependency graph is derived from the repository lockfile, audited
before compilation, and built with one job and `--locked`. Use
`--toolchain-bin DIR` for an installed pinned toolchain and `--target-dir DIR` to
reuse a separate component build cache. Every run retains its generated sources,
input and selected-range hashes, dependency checksums, compiler identities,
commands, logs, executable hash, test inventory and exclusions under
`runs/<timestamp>-<PID>/`; `report.json` points to the latest result. Input and
generated source hashes are checked again after testing. The workload benchmark
remains intentionally ignored unless `--bench` is requested.
