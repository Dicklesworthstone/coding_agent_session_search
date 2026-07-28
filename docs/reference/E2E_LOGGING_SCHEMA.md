# E2E Logging Schema

Unified JSONL schema for all E2E test runs across Rust, Shell scripts, and Playwright.

## Overview

All E2E test infrastructure emits structured JSONL logs to `test-results/e2e/`.
Each line is a self-contained JSON object representing a single event.

## Output Files

| Runner | Output File |
|--------|-------------|
| Rust E2E tests | `test-results/e2e/rust_e2e_<timestamp>.jsonl` |
| Shell scripts | `test-results/e2e/shell_<script>_<timestamp>.jsonl` |
| Playwright | `test-results/e2e/playwright_<timestamp>.jsonl` |

### Per-test CLI trace artifacts

Rust E2E tests that use `PhaseTracker::trace_env_guard()` also route child
`cass` processes to:

`test-results/e2e/<suite>/<test>/trace.jsonl`

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

Both E2E logging acceptance entry points enumerate every `tests/e2e_*.rs`
target explicitly and execute Cargo through fail-closed remote compilation.
They validate only artifacts actually present in the checkout; because RCH does
not currently retrieve arbitrary `test-results/e2e/**` output, a remote full
run without an explicit artifact handoff fails the local nonempty-artifact gate
instead of accepting stale evidence. Run-scoped exact-worker transfer and
manifest verification are tracked by
`coding_agent_session_search-k13gt`. Reports include trace files, bytes, events,
target histograms, receipts, and command outcomes. A failed test target,
malformed line, missing command outcome, empty full-run trace set, per-test
overflow, or semantic aggregate overflow makes acceptance fail.

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
jq -s '[.[] | select(.event == "test_end" and .result.status == "fail")] | length' test-results/e2e/*.jsonl

# Get failed test names
jq -r 'select(.event == "test_end" and .result.status == "fail") | .test.name' test-results/e2e/*.jsonl

# Total duration by runner
jq -s 'group_by(.runner) | map({runner: .[0].runner, total_ms: [.[] | select(.event == "run_end") | .summary.duration_ms] | add})' test-results/e2e/*.jsonl
```

## Backward Compatibility

Existing log formats in `test-logs/` and `target/e2e-cli/` remain unchanged.
This unified schema supplements (not replaces) those formats for CI integration.
