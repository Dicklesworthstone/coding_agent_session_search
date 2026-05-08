#!/usr/bin/env bash
# scripts/tests/run_all.sh
# Orchestrated E2E test runner with unified JSONL logging and consolidated reports.
#
# Usage:
#   ./scripts/tests/run_all.sh              # Run all E2E suites
#   ./scripts/tests/run_all.sh --rust-only  # Run only Rust E2E tests
#   ./scripts/tests/run_all.sh --shell-only # Run only shell script tests
#   ./scripts/tests/run_all.sh --playwright-only # Run only Playwright tests
#   ./scripts/tests/run_all.sh --fail-fast  # Stop on first failure
#   ./scripts/tests/run_all.sh --help       # Show usage
#
# Outputs:
#   test-results/e2e/<suite>/<test>/cass.log - Per-test JSONL logs (Rust E2E)
#   test-results/e2e/*.jsonl    - Per-suite JSONL logs (shell/playwright/orchestrator)
#   test-results/e2e/combined.jsonl - Aggregated JSONL (excludes trace.jsonl)
#   test-results/e2e/summary.md - Human-readable summary
#
# Exit code: 0 if all suites pass, 1 if any suite fails

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUTPUT_DIR="${PROJECT_ROOT}/test-results/e2e"
TIMESTAMP=$(date -u +"%Y%m%d_%H%M%S")

# Source the e2e logging library
# shellcheck disable=SC1091
source "${PROJECT_ROOT}/scripts/lib/e2e_log.sh"

# =============================================================================
# Configuration
# =============================================================================

RUN_RUST=${RUN_RUST:-1}
RUN_SHELL=${RUN_SHELL:-1}
RUN_PLAYWRIGHT=${RUN_PLAYWRIGHT:-1}
FAIL_FAST=${FAIL_FAST:-0}
VERBOSE=${VERBOSE:-0}
RCH_BIN=${RCH_BIN:-rch}
RCH_TARGET_DIR=${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_tests_run_all}

# Colors
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED='' GREEN='' YELLOW='' BLUE='' BOLD='' NC=''
fi

# =============================================================================
# Parse Arguments
# =============================================================================

show_help() {
    cat << 'EOF'
Orchestrated E2E Test Runner

Usage:
    ./scripts/tests/run_all.sh [OPTIONS]

Options:
    --rust-only       Run only Rust E2E tests (rch cargo test e2e_*)
    --shell-only      Run only shell script tests (scripts/test-*.sh)
    --playwright-only Run only Playwright E2E tests
    --fail-fast       Stop execution on first suite failure
    --verbose         Show detailed output from each suite
    --help            Show this help message

Environment:
    RCH_BIN          rch executable to use for Rust E2E tests (default: rch)
    RCH_TARGET_DIR   remote Cargo target dir for Rust E2E tests

Outputs:
    test-results/e2e/<suite>/<test>/cass.log  Per-test JSONL logs (Rust E2E)
    test-results/e2e/*.jsonl     Per-suite JSONL logs following SCHEMA.md
    test-results/e2e/combined.jsonl  Aggregated JSONL from all suites (excludes trace.jsonl)
    test-results/e2e/summary.md  Human-readable Markdown summary

Exit Codes:
    0  All suites passed
    1  One or more suites failed
EOF
}

for arg in "$@"; do
    case "$arg" in
        --rust-only)
            RUN_RUST=1; RUN_SHELL=0; RUN_PLAYWRIGHT=0 ;;
        --shell-only)
            RUN_RUST=0; RUN_SHELL=1; RUN_PLAYWRIGHT=0 ;;
        --playwright-only)
            RUN_RUST=0; RUN_SHELL=0; RUN_PLAYWRIGHT=1 ;;
        --fail-fast)
            FAIL_FAST=1 ;;
        --verbose)
            VERBOSE=1 ;;
        --help|-h)
            show_help; exit 0 ;;
        *)
            echo "Unknown option: $arg"; show_help; exit 1 ;;
    esac
done

# =============================================================================
# Suite Definitions
# =============================================================================

declare -a SUITE_NAMES=()
declare -a SUITE_RESULTS=()
declare -a SUITE_DURATIONS=()
TOTAL_PASSED=0
TOTAL_FAILED=0
TOTAL_SKIPPED=0
OVERALL_START=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

# =============================================================================
# Utility Functions
# =============================================================================

log_section() {
    echo ""
    echo -e "${BOLD}${BLUE}==================================================================${NC}"
    echo -e "${BOLD}${BLUE}  $1${NC}"
    echo -e "${BOLD}${BLUE}==================================================================${NC}"
}

log_result() {
    local suite="$1"
    local status="$2"
    local duration="$3"

    if [[ "$status" == "pass" ]]; then
        echo -e "${GREEN}[PASS]${NC} $suite (${duration}ms)"
    elif [[ "$status" == "skip" ]]; then
        echo -e "${YELLOW}[SKIP]${NC} $suite"
    else
        echo -e "${RED}[FAIL]${NC} $suite (${duration}ms)"
    fi
}

run_suite() {
    local name="$1"
    local runner="$2"
    shift 2
    local cmd=("$@")

    log_section "Running: $name"

    local start_time
    start_time=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

    e2e_phase_start "$name" "Running $runner E2E suite"

    local exit_code=0
    local output_file="${OUTPUT_DIR}/suite_${name}_${TIMESTAMP}.log"

    if [[ "$VERBOSE" -eq 1 ]]; then
        "${cmd[@]}" 2>&1 | tee "$output_file" || exit_code=$?
    else
        "${cmd[@]}" > "$output_file" 2>&1 || exit_code=$?
    fi

    local end_time
    end_time=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    local duration=$((end_time - start_time))

    e2e_phase_end "$name" "$duration"

    SUITE_NAMES+=("$name")
    SUITE_DURATIONS+=("$duration")

    if [[ $exit_code -eq 0 ]]; then
        SUITE_RESULTS+=("pass")
        log_result "$name" "pass" "$duration"
        ((TOTAL_PASSED++))
    else
        SUITE_RESULTS+=("fail")
        log_result "$name" "fail" "$duration"
        ((TOTAL_FAILED++))

        # Log error details
        e2e_error "Suite $name failed with exit code $exit_code" "$name"

        if [[ "$FAIL_FAST" -eq 1 ]]; then
            echo -e "${RED}Stopping due to --fail-fast${NC}"
            return 1
        fi
    fi

    return 0
}

skip_suite() {
    local name="$1"
    log_result "$name" "skip" "0"
    SUITE_NAMES+=("$name")
    SUITE_RESULTS+=("skip")
    SUITE_DURATIONS+=("0")
    ((TOTAL_SKIPPED++))
}

ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "rch binary not found: ${RCH_BIN}" >&2
        return 1
    fi
}

# =============================================================================
# Generate Summary
# =============================================================================

generate_summary() {
    local summary_file="${OUTPUT_DIR}/summary.md"
    local combined_file="${OUTPUT_DIR}/combined.jsonl"

    # Aggregate JSONL files (exclude trace.jsonl and combined.jsonl)
    echo "Aggregating JSONL logs..."
    : > "$combined_file"
    find "$OUTPUT_DIR" -type f \( -name "*.jsonl" -o -name "cass.log" \) \
        ! -name "trace.jsonl" ! -name "combined.jsonl" -print0 \
        | sort -z \
        | xargs -0 cat >> "$combined_file" 2>/dev/null || true

    # Generate Markdown summary
    cat > "$summary_file" << EOF
# E2E Test Summary

**Generated:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")
**Run ID:** ${E2E_RUN_ID:-unknown}

## Results

| Suite | Status | Duration |
|-------|--------|----------|
EOF

    for i in "${!SUITE_NAMES[@]}"; do
        local name="${SUITE_NAMES[$i]}"
        local status="${SUITE_RESULTS[$i]}"
        local duration="${SUITE_DURATIONS[$i]}"

        local status_emoji=""
        case "$status" in
            pass) status_emoji="PASS" ;;
            fail) status_emoji="FAIL" ;;
            skip) status_emoji="SKIP" ;;
        esac

        echo "| $name | $status_emoji | ${duration}ms |" >> "$summary_file"
    done

    local overall_end
    overall_end=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    local total_duration=$((overall_end - OVERALL_START))

    cat >> "$summary_file" << EOF

## Summary

- **Total Suites:** $((TOTAL_PASSED + TOTAL_FAILED + TOTAL_SKIPPED))
- **Passed:** $TOTAL_PASSED
- **Failed:** $TOTAL_FAILED
- **Skipped:** $TOTAL_SKIPPED
- **Total Duration:** ${total_duration}ms

## Log Files

- Combined JSONL: \`test-results/e2e/combined.jsonl\`
EOF

    while IFS= read -r -d '' f; do
        echo "- ${f#"${PROJECT_ROOT}"/}" >> "$summary_file"
    done < <(find "$OUTPUT_DIR" -type f \( -name "*.jsonl" -o -name "cass.log" \) \
        ! -name "trace.jsonl" ! -name "combined.jsonl" -print0 | sort -z)

    echo ""
    echo -e "${BOLD}Summary written to:${NC} $summary_file"
}

# =============================================================================
# Main
# =============================================================================

main() {
    mkdir -p "$OUTPUT_DIR"

    # Initialize orchestrator logging
    e2e_init "orchestrator" "run_all"
    e2e_run_start "" "true" "$([[ $FAIL_FAST -eq 1 ]] && echo true || echo false)"

    echo -e "${BOLD}E2E Test Orchestrator${NC}"
    echo "Output directory: $OUTPUT_DIR"
    echo ""

    local failed=0

    # Rust E2E tests
    if [[ "$RUN_RUST" -eq 1 ]]; then
        rust_tests=()
        if command -v git >/dev/null 2>&1; then
            while IFS= read -r t; do
                [[ -z "$t" ]] && continue
                t="${t#./}"
                t="${t#tests/}"
                t="${t%.rs}"
                rust_tests+=("$t")
            done < <(git ls-files 'tests/e2e_*.rs')
        fi
        if [[ "${#rust_tests[@]}" -eq 0 ]] && [[ -d "${PROJECT_ROOT}/tests" ]]; then
            while IFS= read -r t; do
                [[ -z "$t" ]] && continue
                t="${t#./}"
                t="${t#tests/}"
                t="${t%.rs}"
                rust_tests+=("$t")
            done < <(find "${PROJECT_ROOT}/tests" -maxdepth 1 -name 'e2e_*.rs' -print)
        fi
        if [[ "${#rust_tests[@]}" -eq 0 ]]; then
            echo -e "${YELLOW}No Rust e2e_* tests found, skipping${NC}"
            skip_suite "rust_e2e"
        else
            args=()
            for t in "${rust_tests[@]}"; do
                args+=(--test "$t")
            done
            if ! ensure_rch; then
                failed=1
                SUITE_NAMES+=("rust_e2e")
                SUITE_RESULTS+=("fail")
                SUITE_DURATIONS+=("0")
                ((TOTAL_FAILED++))
                log_result "rust_e2e" "fail" "0"
                [[ "$FAIL_FAST" -eq 1 ]] && { generate_summary; exit 1; }
            elif run_suite "rust_e2e" "rust" "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" E2E_LOG=1 cargo test --all-features --verbose "${args[@]}" -- --test-threads=1 --nocapture; then
                :
            else
                failed=1
                [[ "$FAIL_FAST" -eq 1 ]] && { generate_summary; exit 1; }
            fi
        fi
    else
        skip_suite "rust_e2e"
    fi

    # Shell script tests
    if [[ "$RUN_SHELL" -eq 1 ]]; then
        # Run existing shell E2E scripts
        for script in "${PROJECT_ROOT}"/scripts/test-*-e2e.sh "${PROJECT_ROOT}"/scripts/e2e/*.sh; do
            if [[ -f "$script" && -x "$script" ]]; then
                local script_name
                script_name=$(basename "$script" .sh)
                if run_suite "shell_${script_name}" "shell" "$script"; then
                    :
                else
                    failed=1
                    [[ "$FAIL_FAST" -eq 1 ]] && { generate_summary; exit 1; }
                fi
            fi
        done
    else
        skip_suite "shell_tests"
    fi

    # Playwright E2E tests
    if [[ "$RUN_PLAYWRIGHT" -eq 1 ]]; then
        if [[ -d "${PROJECT_ROOT}/tests/e2e" ]]; then
            if run_suite "playwright" "playwright" npx playwright test --config="${PROJECT_ROOT}/tests/playwright.config.ts"; then
                :
            else
                failed=1
                [[ "$FAIL_FAST" -eq 1 ]] && { generate_summary; exit 1; }
            fi
        else
            echo -e "${YELLOW}Playwright tests not found, skipping${NC}"
            skip_suite "playwright"
        fi
    else
        skip_suite "playwright"
    fi

    # Generate summary
    local overall_end
    overall_end=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    local total_duration=$((overall_end - OVERALL_START))

    e2e_run_end "$((TOTAL_PASSED + TOTAL_FAILED + TOTAL_SKIPPED))" "$TOTAL_PASSED" "$TOTAL_FAILED" "$TOTAL_SKIPPED" "$total_duration"

    generate_summary

    # Final summary
    log_section "Final Results"
    echo -e "Passed:  ${GREEN}$TOTAL_PASSED${NC}"
    echo -e "Failed:  ${RED}$TOTAL_FAILED${NC}"
    echo -e "Skipped: ${YELLOW}$TOTAL_SKIPPED${NC}"
    echo -e "Duration: ${total_duration}ms"
    echo ""

    if [[ $failed -eq 1 ]] || [[ $TOTAL_FAILED -gt 0 ]]; then
        echo -e "${RED}${BOLD}E2E tests failed${NC}"
        exit 1
    else
        echo -e "${GREEN}${BOLD}All E2E tests passed${NC}"
        exit 0
    fi
}

main "$@"
