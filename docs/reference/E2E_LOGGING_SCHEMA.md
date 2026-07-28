# E2E Logging Schema

Unified JSONL schema for all E2E test runs across Rust, Shell scripts, and Playwright.

## Overview

All acceptance evidence is scoped to one validated run ID under
`test-results/e2e/runs/<run_id>/`. Each JSONL line is a self-contained event.
No acceptance command searches the process-global results tree, and a stale
file from another run can never satisfy the current run's gates.

## Output Files

| Runner | Run-scoped output |
|--------|-------------|
| Run executor | `test-results/e2e/runs/<run_id>/run.jsonl` |
| Rust E2E tests | `test-results/e2e/runs/<run_id>/rust_<timestamp>_<nonce>.jsonl` or a per-test `cass.log` |
| Shell scripts | `test-results/e2e/runs/<run_id>/shell_<script>_<timestamp>_<nonce>.jsonl` |
| CLI traces | `test-results/e2e/runs/<run_id>/<suite>/<test>/trace.jsonl` |

The external `CASS_E2E_RUN_ID` is accepted only when it is 8-128 ASCII
alphanumeric, `_`, or `-` bytes, begins and ends alphanumerically, and names a
previously absent directory. Without that variable, developer-focused tests
retain their ordinary non-acceptance output paths.

## Immutable strict-RCH run bundles

`e2e-run-bundle` executes explicit Cargo test targets, captures bounded
stdout/stderr, validates the selected run's JSONL, and writes:

- `manifest.json`: source SHA and diff digest, exact worker ID and hostname,
  complete Cargo command and command digest, timestamps, exit status, every
  file's relative path/SHA-256/byte count/event count/schema, deterministic
  aggregates, and named gate outcomes.
- `complete.json`: a completion receipt binding the exact manifest bytes.

Both control files are created without overwrite and fsynced. Verification
walks the directory again and rejects missing or extra files, changed bytes,
path traversal, symlinks, hard links, duplicate declarations, receipt
tampering, source drift, worker drift, failed tests, failed schema validation,
truncated command capture, empty artifacts, empty traces, or missing aggregate
`run_start`, `test_start`, `test_end`, and `run_end` coverage.

The repository acceptance entry point requires an explicit RCH worker and runs
and finalizes the bundle on that same worker and source. It resolves that
worker's SSH user and host from
`rch --json workers list` and joins that exact host to the unique identity
reported by `rch --json workers discover`; the opaque worker ID is never assumed
to be an SSH alias. It takes the remote manifest path only from the runner's
versioned JSON receipt rather than inferring a project directory from
`remote_base` or searching for whichever artifact happens to exist. It copies
exactly that named remote run into a fresh local staging directory. The
committed acceptance script then independently recomputes the receipt, command
digest, exact file set, regular-file/link policy, every file
hash/size/event count/schema, aggregate counts, and event histogram without
invoking local Cargo. It performs the remaining aggregate gates and only then
publishes the run at
`test-results/e2e/runs/<run_id>/`.
Failed or incomplete staging directories are retained for diagnosis and cannot
be mistaken for published passes.

SSH discovery, probes, and rsync handoff are bounded by a validated
`CASS_E2E_HANDOFF_TIMEOUT_SECONDS` value (900 seconds by default), an SSH
connect timeout, and server-alive failure detection. A timeout is a failed
handoff, never permission to inspect an older local bundle.

The concurrency contract creates the same uniquely named contradictory stale
witness locally and on the explicitly resolved worker before launching either
child run. Both stale copies remain present after the two no-mock runs, while
both independently verified reports and manifests prove they consumed neither
copy. The test never deletes a previous local or remote witness.

The runner emits one flushed `e2e-run-started` JSON receipt after creating its
absent run directory, then one `e2e-run-finalized` receipt only after the
manifest and completion receipt are durable. If execution fails between those
points, the acceptance script uses only the validated started-receipt path to
retrieve and retain the unfinalized directory; it never publishes it.

Before execution, the worker independently recomputes `git rev-parse HEAD`, the
binary diff digest excluding tracker-only `.beads` state, and the untracked path
set. A caller-supplied SHA/diff mismatch or any unreceipted source path outside
`.beads/` and `test-results/` fails before the test command begins.

The command digest has a language-neutral framing so Rust and shell verification
cannot accidentally hash different argument boundaries:

```text
cass-e2e-command-v1\n
<argument-0-byte-count>:<argument-0>\n
<argument-1-byte-count>:<argument-1>\n
...
```

This verifier deliberately remains a small shell/jq/SHA-256 handoff rather
than copying the remote debug runner: the application facade can make that
binary enormous, while the run's raw evidence is small and bounded. Failed
remote jobs therefore remain inspectable even when RCH does not perform its
ordinary successful-build artifact retrieval.

```bash
RCH_WORKER=ovh-a ./scripts/e2e_logging_acceptance_test.sh

# Prove two simultaneous no-mock semantic runs remain isolated from one another
# and from retained contradictory stale runs on both the caller and worker.
RCH_WORKER=ovh-a ./scripts/e2e_logging_acceptance_test.sh \
  --concurrency-contract

# Exercise one explicit target/filter while developing the bundle contract.
RCH_WORKER=ovh-a ./scripts/e2e_logging_acceptance_test.sh \
  --contract-only \
  --contract-target e2e_semantic_search \
  --contract-filter parallel_cass_children_keep_corpus_and_trace_artifacts_isolated

# Re-verify one already-published immutable run; current checkout drift is not
# substituted for the provenance sealed in that run.
./scripts/e2e_logging_acceptance_test.sh \
  --quick \
  --run-id acceptance-20260728T120000Z-a1b2c3
```

### Per-test CLI trace artifacts

Rust E2E tests that use `PhaseTracker::trace_env_guard()` also route child
`cass` processes to:

`test-results/e2e/runs/<run_id>/<suite>/<test>/trace.jsonl`

This file is valid JSONL. It is a CLI diagnostic stream rather than an E2E
runner-event stream, so every record has the tracing envelope
`schema_version, timestamp, level, target, trace_id, test_id, fields`. CASS
targets (`coding_agent_search` and `cass`) retain DEBUG detail, except routine
`cass::redact::memo` hit/miss bookkeeping is pinned to WARN so trace redaction
does not generate self-amplifying diagnostics. Memo invalidation/quarantine
warnings and all other WARN/ERROR events remain visible. This keeps useful
phase and failure evidence while excluding high-volume dependency telemetry
such as per-token SQL parser DEBUG events. The built-in filter also publishes a
DEBUG max-level hint, so TRACE callsites are disabled before event fields are
constructed; per-character tokenizer TRACE instrumentation cannot recreate the
logging storm that this bounded surface is intended to prevent.

The trace is bounded to 512 KiB per test by the E2E guard, and the semantic E2E
suite has a 10 MiB aggregate gate enforced after the run. Production
`--trace-file` output defaults to 16 MiB and 50,000 diagnostic events.
`CASS_TRACE_MAX_BYTES` can override the byte ceiling (clamped to
4 KiB..1 GiB), `CASS_TRACE_MAX_EVENTS` can override the event ceiling (clamped
to 16..10,000,000), and `CASS_TRACE_FILTER` can override the filter using
`tracing_subscriber::EnvFilter` syntax. Appending fails closed when an existing
file is oversized, invalid JSON, has a malformed envelope, or uses another
schema; CASS never truncates or mixes a legacy trace into this artifact. Trace
paths are single-writer:
concurrent processes targeting one file fail cleanly instead of interleaving
records.

If the diagnostic stream reaches its byte or event budget, it remains valid
JSONL and includes a receipt like:

```json
{
  "schema_version": "cass-trace-v1",
  "timestamp": "2026-01-26T12:00:02.500Z",
  "level": "WARN",
  "target": "cass::trace",
  "trace_id": "82a0d1",
  "test_id": "test-results/e2e/e2e_semantic_search/semantic_search_restarts",
  "fields": {
    "event": "trace_truncated",
    "reason": "byte_budget",
    "artifact_complete": false,
    "max_bytes": 524288,
    "max_events": 4096,
    "bytes_written_before_receipt": 456321,
    "events_written_before_receipt": 1802,
    "suppressed_events": 1204,
    "suppressed_bytes": 873102,
    "suppression_reasons": {
      "byte_budget": 1204,
      "event_budget": 0,
      "oversize_event": 0
    },
    "suppressed_targets": [
      {"target": "cass::semantic", "count": 1204}
    ],
    "suppressed_target_overflow_events": 0,
    "failure_tail_events": 3,
    "failure_tail_bytes": 1402,
    "failure_tail_dropped_events": 0,
    "filtered_events": 96077,
    "filtered_targets": [
      {"target": "fsqlite.parse", "count": 96077}
    ],
    "filtered_target_overflow_events": 0
  }
}
```

When only the intentional target filter suppresses events, the corresponding
record is `fields.event="trace_filter_summary"` with
`artifact_complete=true`. Filtered and budget-suppressed target counts are kept
separate and ordered deterministically by count then target. The top eight of
each class are retained (two under very small byte ceilings); the respective
remainder is summed in
`filtered_target_overflow_events` or
`suppressed_target_overflow_events`.
At very small byte ceilings, if even those bounded target lists cannot fit the
reserved tail, CASS writes `receipt_compact=true`, omits the lists, and assigns
all affected counts to the corresponding overflow fields. The receipt still
retains the limit, suppression-reason, filtered-event, and WARN/ERROR-tail
totals.

The byte ceiling reserves a bounded tail for late WARN/ERROR events. When the
ordinary diagnostic head is full, the newest high-severity records are retained
in that tail and the receipt reports how many were written or displaced. After
the head first reaches its byte or event ceiling it remains closed, so the
artifact is a deterministic prefix plus the bounded high-severity tail rather
than a discontinuous sample of later small diagnostics. Reopening the same
artifact for another child command recovers that closed state from the prior
truncation receipt. The separate command summary then records the final outcome
even if no more diagnostic events fit.

Every successfully parsed child invocation that opens the trace artifact
appends a redacted `fields.event="command_summary"` record with command,
duration, exit code, request/trace IDs, and structured failure details.
Secret-bearing fields,
sensitive CLI flag values, recognized token patterns, private home/workspace
paths, emails, and hostnames are redacted. Arguments, correlation values, and
error text are length-bounded; if the detailed summary cannot fit its reserved
tail, a compact `summary_truncated=true` outcome record is written instead.

Both E2E logging acceptance paths execute the same canonical run-bundle entry
point. Reports live outside the immutable bytes under
`test-results/e2e/reports/<run_id>.txt` and include source/worker/command
identity, manifest and receipt hashes, trace files/bytes/events, target
histograms, receipts, command outcomes, and every manifest gate. A failed test
target, malformed line, missing command outcome, incomplete event lifecycle,
empty full-run trace set, per-test overflow, semantic aggregate overflow,
handoff ambiguity, or digest mismatch makes acceptance fail.

## Common Fields (All Events)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `ts` | string | yes | ISO-8601 timestamp with milliseconds |
| `event` | string | yes | Event type (see Event Types below) |
| `run_id` | string | yes | Unique identifier for this test run |
| `runner` | string | yes | `"rust"`, `"shell"`, or `"playwright"` |

## Event Types

### `run_start`

Emitted once at the beginning of a test run.

```json
{
  "ts": "2026-01-26T12:00:00.000Z",
  "event": "run_start",
  "run_id": "20260126_120000_abc123",
  "runner": "rust",
  "env": {
    "git_sha": "abc123def",
    "git_branch": "main",
    "os": "linux",
    "arch": "x86_64",
    "rust_version": "1.84.0",
    "node_version": "24.12.0",
    "cass_version": "0.5.0"
  },
  "config": {
    "test_filter": "e2e_*",
    "parallel": true,
    "fail_fast": false
  }
}
```

### `test_start`

Emitted when a single test begins.

```json
{
  "ts": "2026-01-26T12:00:01.000Z",
  "event": "test_start",
  "run_id": "20260126_120000_abc123",
  "runner": "rust",
  "test": {
    "name": "test_pages_export_basic",
    "suite": "e2e_pages",
    "file": "tests/e2e_pages.rs",
    "line": 42
  }
}
```

### `test_end`

Emitted when a single test completes.

```json
{
  "ts": "2026-01-26T12:00:05.500Z",
  "event": "test_end",
  "run_id": "20260126_120000_abc123",
  "runner": "rust",
  "test": {
    "name": "test_pages_export_basic",
    "suite": "e2e_pages",
    "file": "tests/e2e_pages.rs",
    "line": 42
  },
  "result": {
    "status": "pass",
    "duration_ms": 4500,
    "retries": 0
  }
}
```

**Status values:** `pass`, `fail`, `skip`, `flaky`

### `test_end` (failure)

```json
{
  "ts": "2026-01-26T12:00:10.000Z",
  "event": "test_end",
  "run_id": "20260126_120000_abc123",
  "runner": "rust",
  "test": {
    "name": "test_pages_export_encrypted",
    "suite": "e2e_pages",
    "file": "tests/e2e_pages.rs",
    "line": 87
  },
  "result": {
    "status": "fail",
    "duration_ms": 8000,
    "retries": 1
  },
  "error": {
    "message": "assertion failed: expected 200, got 500",
    "type": "AssertionError",
    "stack": "at tests/e2e_pages.rs:95\n  at ..."
  }
}
```

### `run_end`

Emitted once at the end of a test run with summary statistics.

```json
{
  "ts": "2026-01-26T12:05:00.000Z",
  "event": "run_end",
  "run_id": "20260126_120000_abc123",
  "runner": "rust",
  "summary": {
    "total": 25,
    "passed": 23,
    "failed": 1,
    "skipped": 1,
    "flaky": 0,
    "duration_ms": 300000
  },
  "exit_code": 1
}
```

### `log`

General log message (info, warn, error, debug).

```json
{
  "ts": "2026-01-26T12:00:02.500Z",
  "event": "log",
  "run_id": "20260126_120000_abc123",
  "runner": "shell",
  "level": "INFO",
  "msg": "Building cass binary...",
  "context": {
    "phase": "setup",
    "command": "cargo build --release"
  }
}
```

**Level values:** `DEBUG`, `INFO`, `WARN`, `ERROR`

### `phase_start` / `phase_end`

For multi-phase test runs (setup, execution, teardown).

```json
{
  "ts": "2026-01-26T12:00:00.500Z",
  "event": "phase_start",
  "run_id": "20260126_120000_abc123",
  "runner": "playwright",
  "phase": {
    "name": "global_setup",
    "description": "Building exports and starting preview server"
  }
}
```

### `artifact`

References to generated artifacts (screenshots, logs, exports).

```json
{
  "ts": "2026-01-26T12:00:10.000Z",
  "event": "artifact",
  "run_id": "20260126_120000_abc123",
  "runner": "playwright",
  "artifact": {
    "type": "screenshot",
    "name": "test-failed-1.png",
    "path": "test-results/e2e/screenshots/test-failed-1.png",
    "test_name": "encryption-password-flow"
  }
}
```

## Environment Object

The `env` object in `run_start` captures reproducibility metadata:

| Field | Type | Description |
|-------|------|-------------|
| `git_sha` | string | Current Git commit SHA (short) |
| `git_branch` | string | Current Git branch name |
| `os` | string | Operating system (`linux`, `darwin`, `windows`) |
| `arch` | string | CPU architecture (`x86_64`, `aarch64`) |
| `rust_version` | string? | Rust version if applicable |
| `node_version` | string? | Node.js version if applicable |
| `cass_version` | string | cass binary version |
| `ci` | bool | True if running in CI environment |

## Aggregation

The `scripts/tests/run_all.sh` runner (P6.14j) aggregates all JSONL files:

1. Concatenates all `*.jsonl` files into `test-results/e2e/combined.jsonl`
2. Generates `test-results/e2e/summary.md` with pass/fail table
3. Exits non-zero if any `run_end` has `exit_code != 0`

## Parsing Examples

```bash
# Count failures
find test-results/e2e/runs/<run_id> -type f -name '*.jsonl' \
  -exec jq -s '[.[] | select(.event == "test_end" and .result.status == "fail")] | length' {} +

# Get failed test names
find test-results/e2e/runs/<run_id> -type f -name '*.jsonl' \
  -exec jq -r 'select(.event == "test_end" and .result.status == "fail") | .test.name' {} +

# Total duration by runner
find test-results/e2e/runs/<run_id> -type f -name '*.jsonl' \
  -exec jq -s 'group_by(.runner) | map({runner: .[0].runner, total_ms: [.[] | select(.event == "run_end") | .summary.duration_ms] | add})' {} +
```

## Backward Compatibility

Existing log formats in `test-logs/` and `target/e2e-cli/` remain unchanged.
This unified schema supplements (not replaces) those formats for CI integration.
