#!/usr/bin/env bash
# Comprehensive E2E Logging Acceptance Test
#
# This test verifies the entire E2E logging system works end-to-end by running
# all E2E tests with logging enabled and validating the aggregated output.
#
# Usage: ./scripts/e2e_logging_acceptance_test.sh [--quick]
#
# Options:
#   --quick    Run quick validation only (skip full test run, use existing logs)
#
# Environment:
#   RCH_BIN               rch executable (default: rch)
#   RCH_TEST_TIMEOUT_SEC  strict remote test timeout (default: 7200)
#
# Exit codes:
#   0 - Acceptance test passed
#   1 - Acceptance test failed

set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"
RCH_BIN="${RCH_BIN:-rch}"

# Configuration
TEST_RESULTS_DIR="test-results/e2e"
REPORT_FILE="$TEST_RESULTS_DIR/acceptance_report.txt"
TEST_OUTPUT_FILE=""
QUICK_MODE=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --quick)
            QUICK_MODE=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [--quick]"
            echo ""
            echo "Options:"
            echo "  --quick    Run quick validation only (skip full test run, use existing logs)"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Ensure jq is available
if ! command -v jq &>/dev/null; then
    echo "Error: jq is required but not installed"
    exit 1
fi

ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "Error: rch is required for offloaded Cargo test execution"
        exit 1
    fi
}

run_cargo() {
    RCH_REQUIRE_REMOTE=1 \
        RCH_NO_SELF_HEALING=1 \
        RCH_TEST_TIMEOUT_SEC="${RCH_TEST_TIMEOUT_SEC:-7200}" \
        env -u CARGO_TARGET_DIR \
        "$RCH_BIN" --no-self-healing exec -- cargo "$@"
}

archive_prior_logs() {
    local run_id="$1"
    local archive_dir="$TEST_RESULTS_DIR/.previous/$run_id"
    local moved=0

    mkdir -p "$TEST_RESULTS_DIR"
    while IFS= read -r -d '' file; do
        local rel="${file#"$TEST_RESULTS_DIR"/}"
        local dest="$archive_dir/$rel"
        if [[ -e "$dest" ]]; then
            echo "Error: refusing to overwrite archived log: $dest"
            exit 1
        fi
        mkdir -p "$(dirname "$dest")"
        mv "$file" "$dest"
        moved=$((moved + 1))
    done < <(
        find "$TEST_RESULTS_DIR" \
            -type f \( -name "*.jsonl" -o -name "cass.log" -o -name "acceptance_test_output_*.txt" \) \
            ! -name "combined.jsonl" \
            ! -path "$TEST_RESULTS_DIR/.previous/*" \
            -print0 2>/dev/null
    )

    if [[ "$moved" -gt 0 ]]; then
        echo "Archived $moved existing log file(s) under $archive_dir"
    fi
}

echo "=== E2E Logging Acceptance Test ==="
echo "This test verifies the entire E2E logging system works correctly."
echo ""

# Helper to generate run log events
generate_run_event() {
    local event_type="$1"
    local run_id="$2"
    local ts
    ts=$(date -u +"%Y-%m-%dT%H:%M:%S.000Z")

    case "$event_type" in
        run_start)
            jq -nc --arg ts "$ts" --arg run_id "$run_id" '{
                ts: $ts,
                event: "run_start",
                run_id: $run_id,
                runner: "acceptance_test",
                env: {
                    os: "linux",
                    arch: "x86_64",
                    hostname: "acceptance"
                }
            }'
            ;;
        run_end)
            jq -nc --arg ts "$ts" --arg run_id "$run_id" '{
                ts: $ts,
                event: "run_end",
                run_id: $run_id,
                runner: "acceptance_test",
                summary: {
                    total: 0,
                    passed: 0,
                    failed: 0,
                    skipped: 0
                }
            }'
            ;;
    esac
}

# Step 1: Run E2E tests (unless quick mode)
RUN_LOG="$TEST_RESULTS_DIR/acceptance_run.jsonl"
if [[ "$QUICK_MODE" == false ]]; then
    echo "Step 1: Running E2E tests with logging enabled..."

    E2E_TEST_ARGS=()
    while IFS= read -r test_file; do
        test_target=$(basename "$test_file" .rs)
        E2E_TEST_ARGS+=(--test "$test_target")
    done < <(find tests -maxdepth 1 -type f -name 'e2e_*.rs' | sort)
    if [[ ${#E2E_TEST_ARGS[@]} -eq 0 ]]; then
        echo "FAIL: No tests/e2e_*.rs targets were discovered"
        exit 1
    fi
    echo "  Discovered $((${#E2E_TEST_ARGS[@]} / 2)) explicit E2E test targets"

    # Generate a unique run ID
    RUN_ID="acceptance_$(date +%Y%m%d_%H%M%S)_$(head -c 6 /dev/urandom | xxd -p)"
    archive_prior_logs "$RUN_ID"
    TEST_OUTPUT_FILE="$TEST_RESULTS_DIR/acceptance_test_output_$RUN_ID.txt"

    # Emit run_start event
    generate_run_event "run_start" "$RUN_ID" > "$RUN_LOG"

    # Run E2E tests with logging through rch - capture exit code
    ensure_rch
    set +e
    E2E_LOG=1 run_cargo test --locked "${E2E_TEST_ARGS[@]}" -- --test-threads=1 2>&1 \
        | tee "$TEST_OUTPUT_FILE"
    TEST_EXIT=$?
    set -e

    # Emit run_end event
    generate_run_event "run_end" "$RUN_ID" >> "$RUN_LOG"

    echo ""
    echo "E2E tests completed with exit code: $TEST_EXIT"
else
    echo "Step 1: Quick mode - skipping test run, using existing logs..."
    TEST_EXIT=0
fi

# Step 2: Verify JSONL files were created
echo ""
echo "Step 2: Verifying JSONL files created..."
JSONL_FILES=()
TRACE_FILES=()
while IFS= read -r -d '' file; do
    JSONL_FILES+=("$file")
done < <(
    find "$TEST_RESULTS_DIR" \
        -type f \( -name "*.jsonl" -o -name "cass.log" \) \
        ! -name "trace.jsonl" \
        ! -name "combined.jsonl" \
        ! -path "$TEST_RESULTS_DIR/.previous/*" \
        -print0 2>/dev/null | sort -z
)
while IFS= read -r -d '' file; do
    TRACE_FILES+=("$file")
done < <(
    find "$TEST_RESULTS_DIR" \
        -type f -name "trace.jsonl" \
        ! -path "$TEST_RESULTS_DIR/.previous/*" \
        -print0 2>/dev/null | sort -z
)

JSONL_COUNT=${#JSONL_FILES[@]}
if [[ "$JSONL_COUNT" -eq 0 ]]; then
    echo "FAIL: No JSONL files created in $TEST_RESULTS_DIR"
    echo "  Make sure E2E_LOG=1 is set when running tests"
    exit 1
fi
echo "  Found $JSONL_COUNT JSONL/log files"
echo "  Found ${#TRACE_FILES[@]} versioned per-test trace files"

NONEMPTY_TRACE_COUNT=0
TRACE_BYTES=0
TRACE_EVENTS=0
for trace_file in "${TRACE_FILES[@]}"; do
    if [[ -s "$trace_file" ]]; then
        NONEMPTY_TRACE_COUNT=$((NONEMPTY_TRACE_COUNT + 1))
        TRACE_BYTES=$((TRACE_BYTES + $(wc -c < "$trace_file")))
        TRACE_EVENTS=$((TRACE_EVENTS + $(wc -l < "$trace_file")))
    fi
done
echo "  Non-empty trace files: $NONEMPTY_TRACE_COUNT"
echo "  Persisted trace bytes: $TRACE_BYTES"
echo "  Persisted trace events: $TRACE_EVENTS"
if [[ "$QUICK_MODE" == false && "$NONEMPTY_TRACE_COUNT" -eq 0 ]]; then
    echo "FAIL: Full acceptance run produced no diagnostic trace events"
    exit 1
fi

# Step 3: Validate all JSONL files with schema validator
echo ""
echo "Step 3: Validating JSONL schema..."
if [[ -x "$SCRIPT_DIR/validate-e2e-jsonl.sh" ]]; then
    if ! "$SCRIPT_DIR/validate-e2e-jsonl.sh" "${JSONL_FILES[@]}" "${TRACE_FILES[@]}"; then
        echo "FAIL: JSONL schema validation failed"
        exit 1
    fi
    echo "  Schema validation passed"
else
    echo "  Warning: validate-e2e-jsonl.sh not found, skipping schema validation"
fi

if [[ "$NONEMPTY_TRACE_COUNT" -gt 0 ]]; then
    echo "  Trace target histogram (top 20):"
    jq -r '.target // "unknown"' "${TRACE_FILES[@]}" 2>/dev/null \
        | sort \
        | uniq -c \
        | sort -nr \
        | head -20 \
        | sed 's/^/    /'
    echo "  Trace receipts:"
    jq -r '.fields.event // empty' "${TRACE_FILES[@]}" 2>/dev/null \
        | awk '
            $0 == "trace_truncated" { truncated += 1 }
            $0 == "trace_filter_summary" { filtered += 1 }
            $0 == "command_summary" { commands += 1 }
            END {
                printf "    command_summary=%d trace_filter_summary=%d trace_truncated=%d\n",
                    commands, filtered, truncated
            }
        '
fi

# Step 4: Check event type coverage
echo ""
echo "Step 4: Checking event coverage..."
EVENTS=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq -r '.event' 2>/dev/null | sort -u | grep -v '^$' || true)

# In quick mode, only require test_start/test_end (per-test logs don't have run events)
# In full mode, require all event types including run_start/run_end
if [[ "$QUICK_MODE" == true ]]; then
    REQUIRED_EVENTS="test_start test_end"
    echo "  (Quick mode: only checking for test events)"
else
    REQUIRED_EVENTS="run_start test_start test_end run_end"
fi
MISSING_EVENTS=()

for event in $REQUIRED_EVENTS; do
    if ! echo "$EVENTS" | grep -q "^${event}$"; then
        MISSING_EVENTS+=("$event")
    fi
done

if [[ ${#MISSING_EVENTS[@]} -gt 0 ]]; then
    echo "FAIL: Missing required event types: ${MISSING_EVENTS[*]}"
    exit 1
fi
echo "  All required event types present: $REQUIRED_EVENTS"

# List all event types found
UNIQUE_EVENT_COUNT=$(echo "$EVENTS" | wc -l | tr -d ' ')
echo "  Found $UNIQUE_EVENT_COUNT unique event types: $(echo "$EVENTS" | tr '\n' ' ')"

# Step 5: Check phase event coverage
echo ""
echo "Step 5: Checking phase event coverage..."
PHASE_START_COUNT=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq 'select(.event == "phase_start")' 2>/dev/null | wc -l | tr -d ' ')
PHASE_END_COUNT=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq 'select(.event == "phase_end")' 2>/dev/null | wc -l | tr -d ' ')

echo "  phase_start events: $PHASE_START_COUNT"
echo "  phase_end events: $PHASE_END_COUNT"

PHASE_WARNING=""
if [[ "$PHASE_END_COUNT" -lt 10 ]]; then
    PHASE_WARNING="WARNING: Only $PHASE_END_COUNT phase_end events (target: > 10)"
    echo "  $PHASE_WARNING"
else
    echo "  Phase coverage: OK (> 10 phase events)"
fi

# Step 6: Check metrics event coverage
echo ""
echo "Step 6: Checking metrics coverage..."
METRICS_COUNT=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq 'select(.event == "metrics")' 2>/dev/null | wc -l | tr -d ' ')
echo "  metrics events: $METRICS_COUNT"

METRICS_WARNING=""
if [[ "$METRICS_COUNT" -lt 5 ]]; then
    METRICS_WARNING="WARNING: Only $METRICS_COUNT metrics events (target: > 5)"
    echo "  $METRICS_WARNING"
else
    echo "  Metrics coverage: OK (> 5 metrics events)"
fi

# Step 7: Generate summary report
echo ""
echo "Step 7: Generating summary report..."
TOTAL_EVENTS=$(cat "${JSONL_FILES[@]}" 2>/dev/null | wc -l | tr -d ' ')

mkdir -p "$TEST_RESULTS_DIR"
cat > "$REPORT_FILE" <<EOF
=== E2E Logging Acceptance Report ===
Generated: $(date -Iseconds)

Test Execution
--------------
Exit code: $TEST_EXIT
Quick mode: $QUICK_MODE
Test output: ${TEST_OUTPUT_FILE:-n/a}

File Coverage
-------------
JSONL/log files: $JSONL_COUNT
Total events: $TOTAL_EVENTS
Trace files: ${#TRACE_FILES[@]}
Non-empty trace files: $NONEMPTY_TRACE_COUNT
Trace bytes: $TRACE_BYTES
Trace events: $TRACE_EVENTS

Event Type Coverage
-------------------
Required events: $REQUIRED_EVENTS
All required present: YES
Unique event types: $UNIQUE_EVENT_COUNT

Phase Coverage
--------------
phase_start: $PHASE_START_COUNT
phase_end: $PHASE_END_COUNT
Status: $([ "$PHASE_END_COUNT" -ge 10 ] && echo "PASS (>= 10)" || echo "WARN (< 10)")

Metrics Coverage
----------------
metrics events: $METRICS_COUNT
Status: $([ "$METRICS_COUNT" -ge 5 ] && echo "PASS (>= 5)" || echo "WARN (< 5)")

Files Validated
---------------
$(printf '%s\n' "${JSONL_FILES[@]}")

=== End Report ===
EOF

echo "  Report saved to: $REPORT_FILE"
echo ""
cat "$REPORT_FILE"

# Final result
echo ""
echo "============================================="
if [[ $TEST_EXIT -ne 0 ]]; then
    echo "=== ACCEPTANCE TEST FAILED ==="
    echo "E2E test exit code: $TEST_EXIT"
    exit 1
elif [[ -z "$PHASE_WARNING" ]] && [[ -z "$METRICS_WARNING" ]]; then
    echo "=== ACCEPTANCE TEST PASSED ==="
    exit 0
else
    echo "=== ACCEPTANCE TEST PASSED (with warnings) ==="
    [[ -n "$PHASE_WARNING" ]] && echo "  - $PHASE_WARNING"
    [[ -n "$METRICS_WARNING" ]] && echo "  - $METRICS_WARNING"
    echo ""
    echo "Note: Logging infrastructure is working. Warnings indicate"
    echo "some tests may need additional instrumentation."
    exit 0
fi
