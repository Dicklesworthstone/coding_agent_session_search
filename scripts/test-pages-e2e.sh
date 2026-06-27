#!/usr/bin/env bash
# scripts/test-pages-e2e.sh
# Pages E2E test runner with detailed JSONL logging
#
# Usage:
#   ./scripts/test-pages-e2e.sh           # Run all pages E2E tests
#   ./scripts/test-pages-e2e.sh --verbose # With debug output
#   ./scripts/test-pages-e2e.sh --help    # Show options

set -euo pipefail

# =============================================================================
# Configuration
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="${PROJECT_ROOT}/test-logs"
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")
LOG_FILE="${LOG_DIR}/pages_e2e_${TIMESTAMP}.log"
JSON_LOG_FILE="${LOG_DIR}/pages_e2e_${TIMESTAMP}.jsonl"

# Colors (only when stdout is a terminal)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED='' GREEN='' YELLOW='' BLUE='' CYAN='' BOLD='' NC=''
fi

# Options
VERBOSE=${VERBOSE:-0}
USE_NEXTEST=${USE_NEXTEST:-1}
FAIL_FAST=${FAIL_FAST:-0}
INCLUDE_MASTER=${INCLUDE_MASTER:-0}
RCH_BIN=${RCH_BIN:-rch}
RCH_TARGET_DIR=${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_pages_e2e}

# =============================================================================
# Logging Functions
# =============================================================================

log() {
    local level=$1
    shift
    local msg="$*"
    local timestamp
    timestamp=$(date +"%Y-%m-%d %H:%M:%S.%3N")

    local color
    case $level in
        INFO)  color=$GREEN ;;
        WARN)  color=$YELLOW ;;
        ERROR) color=$RED ;;
        DEBUG) color=$CYAN ;;
        PHASE) color=$BOLD$BLUE ;;
        *)     color=$NC ;;
    esac

    # Console output (colored)
    echo -e "${color}[${timestamp}] [${level}]${NC} ${msg}"

    # Plain text log file
    echo "[${timestamp}] [${level}] ${msg}" >> "$LOG_FILE"

    # JSON log file for CI parsing
    local json_msg
    json_msg=$(echo "$msg" | sed 's/"/\\"/g' | tr '\n' ' ')
    echo "{\"ts\":\"${timestamp}\",\"level\":\"${level}\",\"msg\":\"${json_msg}\"}" >> "$JSON_LOG_FILE"
}

log_section() {
    local title=$1
    echo ""
    echo -e "${BOLD}${BLUE}==================================================================${NC}"
    echo -e "${BOLD}${BLUE}  $title${NC}"
    echo -e "${BOLD}${BLUE}==================================================================${NC}"
    log PHASE "$title"
}

# =============================================================================
# Test Runner Functions
# =============================================================================

ensure_rch() {
    if ! command -v "$RCH_BIN" &> /dev/null; then
        log ERROR "rch binary not found; pages E2E cargo tests must be offloaded"
        return 1
    fi
}

run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

check_nextest() {
    if [[ $USE_NEXTEST -eq 1 ]]; then
        if run_cargo nextest --version > /dev/null 2>&1; then
            return 0
        else
            log WARN "cargo-nextest unavailable through rch, falling back to run_cargo test"
            USE_NEXTEST=0
        fi
    fi
    return 0
}

run_pages_e2e_tests() {
    log_section "Pages E2E Tests"
    local start_time
    start_time=$(date +%s.%N)

    local exit_code=0
    local test_files=("e2e_pages" "pages_pipeline_e2e" "pages_bundle")

    if [[ $INCLUDE_MASTER -eq 1 ]]; then
        test_files+=("pages_master_e2e")
    fi

    for test_file in "${test_files[@]}"; do
        log INFO "Running $test_file..."

        if [[ $USE_NEXTEST -eq 1 ]]; then
            if run_cargo nextest run --profile e2e -E "binary($test_file)" --color=always 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $test_file PASSED"
                echo "{\"test_file\":\"$test_file\",\"status\":\"PASS\"}" >> "$JSON_LOG_FILE"
            else
                log ERROR "  $test_file FAILED"
                echo "{\"test_file\":\"$test_file\",\"status\":\"FAIL\"}" >> "$JSON_LOG_FILE"
                exit_code=1
                [[ $FAIL_FAST -eq 1 ]] && break
            fi
        else
            if run_cargo test --test "$test_file" --color=always -- --nocapture 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $test_file PASSED"
                echo "{\"test_file\":\"$test_file\",\"status\":\"PASS\"}" >> "$JSON_LOG_FILE"
            else
                log ERROR "  $test_file FAILED"
                echo "{\"test_file\":\"$test_file\",\"status\":\"FAIL\"}" >> "$JSON_LOG_FILE"
                exit_code=1
                [[ $FAIL_FAST -eq 1 ]] && break
            fi
        fi
    done

    local end_time
    end_time=$(date +%s.%N)
    local duration
    duration=$(echo "$end_time - $start_time" | bc 2>/dev/null || echo "0")

    if [[ $exit_code -eq 0 ]]; then
        log INFO "All pages E2E tests passed in ${duration}s"
    else
        log ERROR "Some pages E2E tests failed"
    fi

    return $exit_code
}

run_pages_accessibility_tests() {
    log_section "Pages Accessibility Tests"

    if [[ $USE_NEXTEST -eq 1 ]]; then
        if run_cargo nextest run --profile ci -E "binary(pages_accessibility_e2e)" --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "Accessibility tests PASSED"
            return 0
        else
            log ERROR "Accessibility tests FAILED"
            return 1
        fi
    else
        if run_cargo test --test pages_accessibility_e2e --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "Accessibility tests PASSED"
            return 0
        else
            log ERROR "Accessibility tests FAILED"
            return 1
        fi
    fi
}

run_pages_error_handling_tests() {
    log_section "Pages Error Handling Tests"

    if [[ $USE_NEXTEST -eq 1 ]]; then
        if run_cargo nextest run --profile ci -E "binary(pages_error_handling_e2e)" --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "Error handling tests PASSED"
            return 0
        else
            log ERROR "Error handling tests FAILED"
            return 1
        fi
    else
        if run_cargo test --test pages_error_handling_e2e --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "Error handling tests PASSED"
            return 0
        else
            log ERROR "Error handling tests FAILED"
            return 1
        fi
    fi
}

# =============================================================================
# Main
# =============================================================================

show_help() {
    cat << EOF
Usage: $0 [options]

Pages E2E test runner with detailed JSONL logging for CI.

Options:
  -v, --verbose       Verbose output (set RUST_LOG=debug)
  --fail-fast         Stop on first failure
  --include-master    Include master E2E tests (slower)
  --no-nextest        Use run_cargo test instead of run_cargo nextest
  -h, --help          Show this help

Environment Variables:
  VERBOSE=1           Same as --verbose
  FAIL_FAST=1         Same as --fail-fast
  INCLUDE_MASTER=1    Same as --include-master
  USE_NEXTEST=0       Same as --no-nextest
  RCH_BIN=rch         Remote compilation helper binary
  RCH_TARGET_DIR=...  Remote Cargo target dir (default: \${TMPDIR:-/tmp}/rch_target_cass_pages_e2e)

Output:
  Text log: test-logs/pages_e2e_TIMESTAMP.log
  JSON log: test-logs/pages_e2e_TIMESTAMP.jsonl

Examples:
  $0                  # Run standard pages E2E tests
  $0 --include-master # Include comprehensive master tests
  $0 --verbose        # With debug logging

EOF
    exit 0
}

main() {
    mkdir -p "$LOG_DIR"

    # Initialize log files
    echo "# Pages E2E test run - $TIMESTAMP" > "$LOG_FILE"
    echo "" > "$JSON_LOG_FILE"

    # Write test run metadata
    echo "{\"event\":\"test_run_start\",\"timestamp\":\"$TIMESTAMP\",\"project\":\"cass\"}" >> "$JSON_LOG_FILE"

    log_section "PAGES E2E TEST RUNNER"
    log INFO "Project root: $PROJECT_ROOT"
    log INFO "Log directory: $LOG_DIR"
    log INFO "Timestamp: $TIMESTAMP"

    cd "$PROJECT_ROOT"
    ensure_rch || exit 1
    log INFO "RCH binary: $RCH_BIN"
    log INFO "RCH target dir: $RCH_TARGET_DIR"

    # Check for nextest
    check_nextest
    log INFO "Test runner: $([ "$USE_NEXTEST" -eq 1 ] && echo 'run_cargo nextest' || echo 'run_cargo test')"

    # Set verbose logging if requested
    if [[ $VERBOSE -eq 1 ]]; then
        export RUST_LOG=debug
        log INFO "Verbose logging enabled"
    fi

    # Track total results
    local failed=0

    # Run test phases
    run_pages_e2e_tests || ((failed += 1))
    [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]] && exit 1

    run_pages_accessibility_tests || ((failed += 1))
    [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]] && exit 1

    run_pages_error_handling_tests || ((failed += 1))

    # Summary
    log_section "SUMMARY"

    echo ""
    echo -e "${BOLD}Log files:${NC}"
    echo "  Text: $LOG_FILE"
    echo "  JSON: $JSON_LOG_FILE"
    echo ""

    # Write final status
    local final_status="PASS"
    [[ $failed -gt 0 ]] && final_status="FAIL"
    echo "{\"event\":\"test_run_end\",\"status\":\"$final_status\",\"failed_phases\":$failed}" >> "$JSON_LOG_FILE"

    if [[ $failed -gt 0 ]]; then
        log ERROR "TESTS FAILED ($failed phase(s) failed)"
        exit 1
    fi

    log INFO "ALL PAGES E2E TESTS PASSED"
    exit 0
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -v|--verbose)     VERBOSE=1; shift ;;
        --fail-fast)      FAIL_FAST=1; shift ;;
        --include-master) INCLUDE_MASTER=1; shift ;;
        --no-nextest)     USE_NEXTEST=0; shift ;;
        -h|--help)        show_help ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

main
