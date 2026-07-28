#!/usr/bin/env bash
# scripts/e2e/e2e_logging_acceptance_test.sh
#
# Comprehensive E2E Logging Acceptance Test
#
# This script validates that the entire E2E logging infrastructure works
# correctly by running tests with logging enabled and verifying outputs.
#
# br: coding_agent_session_search-3koo
#
# Environment:
#   RCH_BIN               rch executable (default: rch)
#   RCH_TEST_TIMEOUT_SEC  strict remote test timeout (default: 7200)

set -euo pipefail

# Get script directory and project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"

# The repository-root entry point owns the single run-bundle contract. Keep this
# historical path as an exact alias so the two paths cannot drift back into
# independently globbing process-global evidence.
exec "$PROJECT_ROOT/scripts/e2e_logging_acceptance_test.sh" "$@"

# Source the E2E logging library
# shellcheck disable=SC1091
source "$PROJECT_ROOT/scripts/lib/e2e_log.sh"

# Initialize logging for this script
e2e_init "shell" "e2e_logging_acceptance"
e2e_run_start "acceptance" "false" "false"

echo "=== E2E Logging Acceptance Test ==="
echo "This test verifies the entire E2E logging system works correctly."
echo "Run ID: $(e2e_run_id)"
echo "Output: $(e2e_output_file)"
echo ""

# Track test results
TOTAL_CHECKS=0
PASSED_CHECKS=0
FAILED_CHECKS=0
TEST_OUTPUT_FILE=""

ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "  [FAIL] rch binary not found; E2E logging acceptance tests must be offloaded"
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

check_pass() {
    local name="$1"
    ((TOTAL_CHECKS += 1))
    ((PASSED_CHECKS += 1))
    echo "  [PASS] $name"
}

check_fail() {
    local name="$1"
    local reason="$2"
    ((TOTAL_CHECKS += 1))
    ((FAILED_CHECKS += 1))
    echo "  [FAIL] $name: $reason"
}

# =============================================================================
# Step 1: Archive previous results
# =============================================================================
e2e_phase_start "archive" "Archive previous test results"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 1: Archiving previous results..."
PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "archive" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 2: Run E2E tests with logging enabled
# =============================================================================
e2e_phase_start "run_tests" "Run E2E tests with logging enabled"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 2: Running E2E tests with logging enabled..."
E2E_TEST_ARGS=()
while IFS= read -r test_file; do
    test_target=$(basename "$test_file" .rs)
    E2E_TEST_ARGS+=(--test "$test_target")
done < <(find "$PROJECT_ROOT/tests" -maxdepth 1 -type f -name 'e2e_*.rs' | sort)
if [ "${#E2E_TEST_ARGS[@]}" -eq 0 ]; then
    echo "  [FAIL] No tests/e2e_*.rs targets were discovered"
    exit 1
fi
echo "  Running $((${#E2E_TEST_ARGS[@]} / 2)) explicit E2E targets through strict rch"

TEST_EXIT=0
ensure_rch
TEST_OUTPUT_FILE="$PROJECT_ROOT/test-results/e2e/acceptance_test_output_$(e2e_run_id).txt"
E2E_LOG=1 run_cargo test --locked "${E2E_TEST_ARGS[@]}" -- --test-threads=1 2>&1 \
    | tee "$TEST_OUTPUT_FILE" || TEST_EXIT=$?

if [ "$TEST_EXIT" -eq 0 ]; then
    echo "  All E2E tests passed"
else
    echo "  Some E2E tests failed (exit code: $TEST_EXIT)"
    echo "  Note: We continue to validate logging infrastructure"
fi

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "run_tests" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 3: Verify JSONL files were created
# =============================================================================
e2e_phase_start "verify_files" "Verify JSONL files created"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 3: Verifying JSONL files created..."

JSONL_COUNT=$(
    find "$PROJECT_ROOT/test-results/e2e" \
        -name "*.jsonl" \
        -type f \
        ! -path "$PROJECT_ROOT/test-results/e2e/.previous/*" \
        2>/dev/null | wc -l
)
echo "  Found $JSONL_COUNT JSONL files"

if [ "$JSONL_COUNT" -eq 0 ]; then
    check_fail "jsonl_files_exist" "No JSONL files created"
    e2e_error "No JSONL files found in test-results/e2e/"
else
    check_pass "jsonl_files_exist ($JSONL_COUNT files)"
fi

TRACE_COUNT=$(
    find "$PROJECT_ROOT/test-results/e2e" \
        -name "trace.jsonl" \
        -type f \
        ! -path "$PROJECT_ROOT/test-results/e2e/.previous/*" \
        2>/dev/null | wc -l
)
NONEMPTY_TRACE_COUNT=$(
    find "$PROJECT_ROOT/test-results/e2e" \
        -name "trace.jsonl" \
        -type f \
        -size +0c \
        ! -path "$PROJECT_ROOT/test-results/e2e/.previous/*" \
        2>/dev/null | wc -l
)
TRACE_BYTES=$(
    find "$PROJECT_ROOT/test-results/e2e" \
        -name "trace.jsonl" \
        -type f \
        ! -path "$PROJECT_ROOT/test-results/e2e/.previous/*" \
        -printf '%s\n' 2>/dev/null \
        | awk '{ total += $1 } END { print total + 0 }'
)
TRACE_EVENTS=$(
    find "$PROJECT_ROOT/test-results/e2e" \
        -name "trace.jsonl" \
        -type f \
        -size +0c \
        ! -path "$PROJECT_ROOT/test-results/e2e/.previous/*" \
        -print0 2>/dev/null \
        | xargs -0 -r wc -l \
        | awk '$2 != "total" { total += $1 } END { print total + 0 }'
)
echo "  Trace files: $TRACE_COUNT"
echo "  Non-empty trace files: $NONEMPTY_TRACE_COUNT"
echo "  Persisted trace bytes: $TRACE_BYTES"
echo "  Persisted trace events: $TRACE_EVENTS"
if [ "$NONEMPTY_TRACE_COUNT" -eq 0 ]; then
    check_fail "trace_files_nonempty" "Full E2E run produced no diagnostic trace events"
else
    check_pass "trace_files_nonempty ($NONEMPTY_TRACE_COUNT files)"
fi

# List the files found
echo "  Files:"
find "$PROJECT_ROOT/test-results/e2e" \
    -name "*.jsonl" \
    -type f \
    ! -path "$PROJECT_ROOT/test-results/e2e/.previous/*" \
    2>/dev/null | head -10 | while read -r f; do
    echo "    - $(basename "$f")"
done

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "verify_files" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 4: Validate JSONL schema
# =============================================================================
e2e_phase_start "validate_schema" "Validate JSONL schema"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 4: Validating JSONL schema..."

SCHEMA_EXIT=0
"$PROJECT_ROOT/scripts/validate-e2e-jsonl.sh" || SCHEMA_EXIT=$?
RUST_SCHEMA_EXIT=0
run_cargo test --locked --test e2e_jsonl_schema_test 2>&1 | tail -20 || RUST_SCHEMA_EXIT=$?
if [ "$RUST_SCHEMA_EXIT" -ne 0 ]; then
    SCHEMA_EXIT=$RUST_SCHEMA_EXIT
fi

if [ "$SCHEMA_EXIT" -eq 0 ]; then
    check_pass "jsonl_schema_and_trace_budget_valid"
else
    check_fail "jsonl_schema_and_trace_budget_valid" "Schema or trace budget validation failed"
    e2e_error "JSONL schema or trace budget validation failed"
fi

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "validate_schema" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 5: Check event coverage
# =============================================================================
e2e_phase_start "check_events" "Check event type coverage"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 5: Checking event coverage..."

# Collect all events from JSONL files
EVENTS=$(cat "$PROJECT_ROOT"/test-results/e2e/*.jsonl 2>/dev/null | jq -r '.event' 2>/dev/null | sort -u || echo "")

# Check for required event types
REQUIRED_EVENTS="run_start test_start test_end run_end"
MISSING_EVENTS=""

for event in $REQUIRED_EVENTS; do
    if echo "$EVENTS" | grep -q "^$event$"; then
        check_pass "event_$event"
    else
        check_fail "event_$event" "Missing required event type"
        MISSING_EVENTS="$MISSING_EVENTS $event"
    fi
done

if [ -z "$MISSING_EVENTS" ]; then
    echo "  All required event types present"
else
    echo "  Missing events:$MISSING_EVENTS"
fi

# Show all event types found
echo "  Event types found: $(echo "$EVENTS" | tr '\n' ', ' | sed 's/,$//')"

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "check_events" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 6: Check phase event coverage
# =============================================================================
e2e_phase_start "check_phases" "Check phase event coverage"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 6: Checking phase event coverage..."

PHASE_START_COUNT=$(cat "$PROJECT_ROOT"/test-results/e2e/*.jsonl 2>/dev/null | jq 'select(.event == "phase_start")' 2>/dev/null | wc -l || echo "0")
PHASE_END_COUNT=$(cat "$PROJECT_ROOT"/test-results/e2e/*.jsonl 2>/dev/null | jq 'select(.event == "phase_end")' 2>/dev/null | wc -l || echo "0")

echo "  Phase start events: $PHASE_START_COUNT"
echo "  Phase end events: $PHASE_END_COUNT"

if [ "$PHASE_END_COUNT" -ge 10 ]; then
    check_pass "phase_coverage (>= 10 phase_end events)"
elif [ "$PHASE_END_COUNT" -gt 0 ]; then
    echo "  WARNING: Only $PHASE_END_COUNT phase_end events found (expected >= 10)"
    check_pass "phase_coverage ($PHASE_END_COUNT phase_end events, want >= 10)"
else
    check_fail "phase_coverage" "No phase_end events found"
fi

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "check_phases" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 7: Check metrics coverage
# =============================================================================
e2e_phase_start "check_metrics" "Check metrics event coverage"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 7: Checking metrics coverage..."

METRICS_COUNT=$(cat "$PROJECT_ROOT"/test-results/e2e/*.jsonl 2>/dev/null | jq 'select(.event == "metrics")' 2>/dev/null | wc -l || echo "0")

echo "  Metrics events: $METRICS_COUNT"

if [ "$METRICS_COUNT" -ge 5 ]; then
    check_pass "metrics_coverage (>= 5 metrics events)"
elif [ "$METRICS_COUNT" -gt 0 ]; then
    echo "  WARNING: Only $METRICS_COUNT metrics events found (expected >= 5)"
    check_pass "metrics_coverage ($METRICS_COUNT metrics events, want >= 5)"
else
    # Metrics are optional, just note the absence
    echo "  INFO: No metrics events found (optional)"
fi

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "check_metrics" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Step 8: Generate summary report
# =============================================================================
e2e_phase_start "generate_report" "Generate summary report"
PHASE_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

echo ""
echo "Step 8: Generating summary report..."

REPORT_FILE="$PROJECT_ROOT/test-results/e2e/acceptance_report.txt"
TOTAL_EVENTS=$(cat "$PROJECT_ROOT"/test-results/e2e/*.jsonl 2>/dev/null | wc -l || echo "0")

cat > "$REPORT_FILE" << EOF
=== E2E Logging Acceptance Report ===
Generated: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
Run ID: $(e2e_run_id)

Test Execution
--------------
Exit code: $TEST_EXIT
Test output: ${TEST_OUTPUT_FILE:-n/a}
JSONL files: $JSONL_COUNT
Total events: $TOTAL_EVENTS
Trace files: $TRACE_COUNT
Non-empty trace files: $NONEMPTY_TRACE_COUNT
Trace bytes: $TRACE_BYTES
Trace events: $TRACE_EVENTS

Event Coverage
--------------
Event types: $(echo "$EVENTS" | tr '\n' ', ' | sed 's/,$//')
Phase start events: $PHASE_START_COUNT
Phase end events: $PHASE_END_COUNT
Metrics events: $METRICS_COUNT

Validation Checks
-----------------
Total: $TOTAL_CHECKS
Passed: $PASSED_CHECKS
Failed: $FAILED_CHECKS

EOF

cat "$REPORT_FILE"

PHASE_END=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
e2e_phase_end "generate_report" "$((PHASE_END - PHASE_START))"

# =============================================================================
# Final Result
# =============================================================================
TOTAL_DURATION=$(e2e_duration_since_start)

echo ""
echo "========================================="

# Determine final status
# The acceptance test passes if:
# 1. JSONL files were created
# 2. Required event types are present
# 3. Schema validation passes
LOGGING_OK=true

if [ "$JSONL_COUNT" -eq 0 ]; then
    LOGGING_OK=false
fi

if [ "$NONEMPTY_TRACE_COUNT" -eq 0 ]; then
    LOGGING_OK=false
fi

if [ -n "$MISSING_EVENTS" ]; then
    LOGGING_OK=false
fi

if [ "$SCHEMA_EXIT" -ne 0 ]; then
    LOGGING_OK=false
fi

if [ "$TEST_EXIT" -ne 0 ]; then
    LOGGING_OK=false
fi

if [ "$LOGGING_OK" = true ]; then
    echo "=== ACCEPTANCE TEST PASSED ==="
    echo "E2E logging infrastructure is working correctly."
    e2e_run_end "$TOTAL_CHECKS" "$PASSED_CHECKS" "$FAILED_CHECKS" "0" "$TOTAL_DURATION"
    exit 0
else
    echo "=== ACCEPTANCE TEST FAILED ==="
    echo "E2E logging infrastructure has issues that need attention."
    e2e_run_end "$TOTAL_CHECKS" "$PASSED_CHECKS" "$FAILED_CHECKS" "1" "$TOTAL_DURATION"
    exit 1
fi
