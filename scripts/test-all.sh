#!/usr/bin/env bash
# scripts/test-all.sh
# Comprehensive test runner with detailed logging for cass
#
# Usage:
#   ./scripts/test-all.sh           # Run all standard tests
#   ./scripts/test-all.sh --all     # Include SSH and slow tests
#   ./scripts/test-all.sh --help    # Show all options

set -euo pipefail

# =============================================================================
# Configuration
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="${PROJECT_ROOT}/test-logs"
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")
LOG_FILE="${LOG_DIR}/test_${TIMESTAMP}.log"
JSON_LOG_FILE="${LOG_DIR}/test_${TIMESTAMP}.jsonl"

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

# Options with defaults
VERBOSE=${VERBOSE:-0}
PARALLEL=${PARALLEL:-1}
INCLUDE_SSH=${INCLUDE_SSH:-0}
INCLUDE_SLOW=${INCLUDE_SLOW:-0}
USE_NEXTEST=${USE_NEXTEST:-1}
FAIL_FAST=${FAIL_FAST:-0}
QUICK_MODE=${QUICK_MODE:-0}
RCH_BIN=${RCH_BIN:-rch}
RCH_TARGET_DIR=${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_test_all}

# Results tracking
declare -A TIMINGS
declare -A RESULTS

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

log_subsection() {
    local title=$1
    echo ""
    echo -e "${CYAN}--- $title ---${NC}"
    log INFO "--- $title ---"
}

# =============================================================================
# Timing Functions
# =============================================================================

time_start() {
    local name=$1
    TIMINGS["${name}_start"]=$(date +%s.%N)
}

time_end() {
    local name=$1
    local start=${TIMINGS["${name}_start"]}
    local end
    end=$(date +%s.%N)
    local duration
    duration=$(echo "$end - $start" | bc 2>/dev/null || echo "0")
    TIMINGS["${name}_duration"]=$duration
    log INFO "Completed in ${duration}s"
}

# =============================================================================
# Test Runner Functions
# =============================================================================

ensure_rch() {
    if ! command -v "$RCH_BIN" &> /dev/null; then
        log ERROR "rch binary not found; test-all cargo tests must be offloaded"
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

# =============================================================================
# Test Phases
# =============================================================================

run_unit_tests() {
    log_section "PHASE 1: Unit Tests"
    time_start "unit_tests"

    local exit_code=0

    if [[ $USE_NEXTEST -eq 1 ]]; then
        if run_cargo nextest run --profile ci --workspace -E 'kind(lib)' --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "Unit tests passed"
        else
            log ERROR "Unit tests failed"
            exit_code=1
        fi
    else
        if run_cargo test --lib --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "Unit tests passed"
        else
            log ERROR "Unit tests failed"
            exit_code=1
        fi
    fi

    time_end "unit_tests"
    RESULTS["unit_tests"]=$exit_code
    return $exit_code
}

run_connector_tests() {
    if [[ $QUICK_MODE -eq 1 ]]; then
        log_section "PHASE 2: Connector Tests (SKIPPED - quick mode)"
        RESULTS["connector_tests"]="skipped"
        return 0
    fi

    log_section "PHASE 2: Connector Tests"
    time_start "connector_tests"

    local connectors=("claude" "codex" "gemini" "cline" "aider" "amp" "opencode")
    local failed=0

    for conn in "${connectors[@]}"; do
        log_subsection "Testing connector: $conn"
        local filter="binary(connector_${conn})"

        if [[ $USE_NEXTEST -eq 1 ]]; then
            if run_cargo nextest run --profile ci -E "$filter" --color=always 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $conn connector passed"
            else
                log ERROR "  $conn connector failed"
                ((failed += 1))
            fi
        else
            if run_cargo test --test "connector_${conn}" --color=always 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $conn connector passed"
            else
                log ERROR "  $conn connector failed"
                ((failed += 1))
            fi
        fi

        if [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]]; then
            break
        fi
    done

    time_end "connector_tests"

    if [[ $failed -gt 0 ]]; then
        log ERROR "$failed connector test(s) failed"
        RESULTS["connector_tests"]=1
        return 1
    fi

    log INFO "All connector tests passed"
    RESULTS["connector_tests"]=0
    return 0
}

run_cli_tests() {
    log_section "PHASE 3: CLI E2E Tests"
    time_start "cli_tests"

    local test_files=("e2e_cli_flows" "e2e_sources" "e2e_filters" "cli_robot")
    local failed=0

    for test in "${test_files[@]}"; do
        log_subsection "Running $test"

        if [[ $USE_NEXTEST -eq 1 ]]; then
            if run_cargo nextest run --profile e2e -E "binary($test)" --color=always 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $test passed"
            else
                log ERROR "  $test failed"
                ((failed += 1))
            fi
        else
            if run_cargo test --test "$test" --color=always -- --test-threads=1 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $test passed"
            else
                log ERROR "  $test failed"
                ((failed += 1))
            fi
        fi

        if [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]]; then
            break
        fi
    done

    time_end "cli_tests"

    if [[ $failed -gt 0 ]]; then
        log ERROR "$failed CLI test(s) failed"
        RESULTS["cli_tests"]=1
        return 1
    fi

    log INFO "All CLI tests passed"
    RESULTS["cli_tests"]=0
    return 0
}

run_ui_tests() {
    log_section "PHASE 4: UI Component Tests"
    time_start "ui_tests"

    local exit_code=0

    if [[ $USE_NEXTEST -eq 1 ]]; then
        # UI tests are run with the ci profile which has proper thread overrides
        if run_cargo nextest run --profile ci -E 'test(ui_)' --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "UI tests passed"
        else
            log ERROR "UI tests failed"
            exit_code=1
        fi
    else
        if run_cargo test --tests -- ui_ --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "UI tests passed"
        else
            log ERROR "UI tests failed"
            exit_code=1
        fi
    fi

    time_end "ui_tests"
    RESULTS["ui_tests"]=$exit_code
    return $exit_code
}

run_ssh_tests() {
    if [[ $INCLUDE_SSH -eq 0 ]]; then
        log_section "PHASE 5: SSH Tests (SKIPPED - use --include-ssh to run)"
        RESULTS["ssh_tests"]="skipped"
        return 0
    fi

    log_section "PHASE 5: SSH Integration Tests"
    time_start "ssh_tests"

    # Check Docker is available
    if ! command -v docker &> /dev/null; then
        log WARN "Docker not available, skipping SSH tests"
        RESULTS["ssh_tests"]="skipped"
        return 0
    fi

    local exit_code=0

    # Run SSH tests (marked with #[ignore] so need --ignored flag)
    if [[ $USE_NEXTEST -eq 1 ]]; then
        if run_cargo nextest run --profile e2e -E 'test(ssh)' --run-ignored=all --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "SSH tests passed"
        else
            log ERROR "SSH tests failed"
            exit_code=1
        fi
    else
        if run_cargo test ssh -- --ignored --color=always 2>&1 | tee -a "$LOG_FILE"; then
            log INFO "SSH tests passed"
        else
            log ERROR "SSH tests failed"
            exit_code=1
        fi
    fi

    time_end "ssh_tests"
    RESULTS["ssh_tests"]=$exit_code
    return $exit_code
}

run_slow_tests() {
    if [[ $INCLUDE_SLOW -eq 0 ]]; then
        log_section "PHASE 6: Slow Tests (SKIPPED - use --include-slow to run)"
        RESULTS["slow_tests"]="skipped"
        return 0
    fi

    log_section "PHASE 6: Slow/Performance Tests"
    time_start "slow_tests"

    local test_files=("watch_e2e" "regression_behavioral")
    local failed=0

    for test in "${test_files[@]}"; do
        log_subsection "Running $test"

        if [[ $USE_NEXTEST -eq 1 ]]; then
            if run_cargo nextest run --profile e2e -E "binary($test)" --color=always 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $test passed"
            else
                log ERROR "  $test failed"
                ((failed += 1))
            fi
        else
            if run_cargo test --test "$test" --color=always 2>&1 | tee -a "$LOG_FILE"; then
                log INFO "  $test passed"
            else
                log ERROR "  $test failed"
                ((failed += 1))
            fi
        fi
    done

    time_end "slow_tests"

    if [[ $failed -gt 0 ]]; then
        log ERROR "$failed slow test(s) failed"
        RESULTS["slow_tests"]=1
        return 1
    fi

    log INFO "All slow tests passed"
    RESULTS["slow_tests"]=0
    return 0
}

# =============================================================================
# Summary Report
# =============================================================================

print_summary() {
    log_section "TEST SUMMARY"

    echo ""
    printf "${BOLD}%-25s %12s %10s${NC}\n" "Phase" "Duration" "Status"
    printf "%-25s %12s %10s\n" "-------------------------" "------------" "----------"

    local phases=("unit_tests" "connector_tests" "cli_tests" "ui_tests" "ssh_tests" "slow_tests")
    local total_failed=0

    for phase in "${phases[@]}"; do
        local duration=${TIMINGS["${phase}_duration"]:-"-"}
        local status=${RESULTS[$phase]:-"?"}

        local status_text status_color
        if [[ $status == "0" ]]; then
            status_text="PASS"
            status_color=$GREEN
        elif [[ $status == "skipped" ]]; then
            status_text="SKIP"
            status_color=$YELLOW
            duration="-"
        else
            status_text="FAIL"
            status_color=$RED
            ((total_failed += 1))
        fi

        local duration_display="${duration}s"
        [[ $duration == "-" ]] && duration_display="-"
        printf "%-25s %12s ${status_color}%10s${NC}\n" "$phase" "$duration_display" "$status_text"
    done

    echo ""
    echo -e "${BOLD}Log files:${NC}"
    echo "  Text: $LOG_FILE"
    echo "  JSON: $JSON_LOG_FILE"

    if [[ $USE_NEXTEST -eq 1 ]]; then
        local junit_path="${PROJECT_ROOT}/target/nextest/ci/junit.xml"
        if [[ -f "$junit_path" ]]; then
            echo "  JUnit: $junit_path"
        fi
    fi

    echo ""
    return $total_failed
}

# =============================================================================
# Main
# =============================================================================

show_help() {
    cat << EOF
Usage: $0 [options]

Comprehensive test runner for cass with detailed logging.

Options:
  -v, --verbose       Verbose output
  -q, --quick         Quick mode (skip connector and slow tests)
  --no-parallel       Run tests sequentially
  --fail-fast         Stop on first failure
  --include-ssh       Include SSH integration tests (requires Docker)
  --include-slow      Include slow/performance tests
  --all               Include all optional tests
  --no-nextest        Use run_cargo test instead of run_cargo nextest
  -h, --help          Show this help

Environment Variables:
  VERBOSE=1           Same as --verbose
  QUICK_MODE=1        Same as -q/--quick
  PARALLEL=0          Same as --no-parallel
  INCLUDE_SSH=1       Same as --include-ssh
  INCLUDE_SLOW=1      Same as --include-slow
  USE_NEXTEST=0       Same as --no-nextest
  RCH_BIN=rch         Remote compilation helper binary
  RCH_TARGET_DIR=...  Remote Cargo target dir (default: \${TMPDIR:-/tmp}/rch_target_cass_test_all)

Examples:
  $0                  # Standard test run
  $0 --all            # Full test suite including SSH and slow tests
  $0 --fail-fast      # Stop on first failure
  $0 -q               # Quick tests only

EOF
    exit 0
}

main() {
    mkdir -p "$LOG_DIR"

    # Initialize log files
    echo "# cass test run - $TIMESTAMP" > "$LOG_FILE"
    echo "" > "$JSON_LOG_FILE"

    log_section "CASS TEST RUNNER"
    log INFO "Project root: $PROJECT_ROOT"
    log INFO "Log directory: $LOG_DIR"
    log INFO "Timestamp: $TIMESTAMP"
    log INFO "Options: QUICK_MODE=$QUICK_MODE PARALLEL=$PARALLEL INCLUDE_SSH=$INCLUDE_SSH INCLUDE_SLOW=$INCLUDE_SLOW"

    cd "$PROJECT_ROOT"
    ensure_rch || exit 1
    log INFO "RCH binary: $RCH_BIN"
    log INFO "RCH target dir: $RCH_TARGET_DIR"

    # Check for nextest
    check_nextest
    log INFO "Test runner: $([ "$USE_NEXTEST" -eq 1 ] && echo 'run_cargo nextest' || echo 'run_cargo test')"

    # Run all phases, collecting results
    local failed=0

    run_unit_tests || ((failed += 1))
    [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]] && { print_summary; exit 1; }

    run_connector_tests || ((failed += 1))
    [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]] && { print_summary; exit 1; }

    run_cli_tests || ((failed += 1))
    [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]] && { print_summary; exit 1; }

    run_ui_tests || ((failed += 1))
    [[ $FAIL_FAST -eq 1 ]] && [[ $failed -gt 0 ]] && { print_summary; exit 1; }

    run_ssh_tests || ((failed += 1))
    run_slow_tests || ((failed += 1))

    print_summary
    local summary_failed=$?

    if [[ $summary_failed -gt 0 ]]; then
        log ERROR "TESTS FAILED ($summary_failed phase(s) failed)"
        exit 1
    fi

    log INFO "ALL TESTS PASSED"
    exit 0
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -v|--verbose)    VERBOSE=1; shift ;;
        -q|--quick)      QUICK_MODE=1; shift ;;
        --no-parallel)   PARALLEL=0; shift ;;
        --fail-fast)     FAIL_FAST=1; shift ;;
        --include-ssh)   INCLUDE_SSH=1; shift ;;
        --include-slow)  INCLUDE_SLOW=1; shift ;;
        --all)           INCLUDE_SSH=1; INCLUDE_SLOW=1; shift ;;
        --no-nextest)    USE_NEXTEST=0; shift ;;
        -h|--help)       show_help ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

main
