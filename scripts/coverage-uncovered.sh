#!/usr/bin/env bash
# coverage-uncovered.sh - Show uncovered code and functions
#
# Useful for identifying untested code paths that need attention.
#
# Usage:
#   ./scripts/coverage-uncovered.sh           # Show uncovered lines
#   ./scripts/coverage-uncovered.sh --fail    # Exit 1 if below threshold
#
# Environment:
#   RCH_BIN         rch executable (default: rch)
#   RCH_TARGET_DIR  cargo target dir for offloaded coverage work

set -euo pipefail

THRESHOLD=60
FAIL_MODE=false
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_coverage_uncovered}"

for arg in "$@"; do
    case $arg in
        --fail)
            FAIL_MODE=true
            ;;
        --threshold=*)
            THRESHOLD="${arg#*=}"
            ;;
        --help|-h)
            echo "Usage: $0 [--fail] [--threshold=N]"
            echo ""
            echo "Options:"
            echo "  --fail          Exit with code 1 if coverage below threshold"
            echo "  --threshold=N   Set coverage threshold (default: 60)"
            echo ""
            exit 0
            ;;
    esac
done

ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "Error: rch binary not found; coverage Cargo work must be offloaded"
        exit 1
    fi
}

run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

# Check if cargo-llvm-cov is installed on the offloaded toolchain
ensure_rch
if ! run_cargo llvm-cov --version >/dev/null 2>&1; then
    echo "Error: cargo-llvm-cov is not available through rch"
    echo ""
    echo "Install it on the remote toolchain with:"
    echo "  rustup component add llvm-tools-preview"
    echo "  cargo install cargo-llvm-cov"
    echo ""
    exit 1
fi

# Check if jq is installed (needed for percentage calculation)
if ! command -v jq &> /dev/null; then
    echo "Warning: jq not installed - coverage percentage will not be displayed"
    echo "Install with: brew install jq (macOS) or apt install jq (Linux)"
    echo ""
fi

echo "Analyzing uncovered code..."
echo ""

# Common options for all coverage runs
COMMON_OPTS=(
    --all-features
    --workspace
    --ignore-filename-regex='(tests/|benches/|\.cargo/)'
)

# Test exclusions (same as CI)
TEST_OPTS=(
    --
    --skip install_sh
    --skip install_ps1
)

# Clean previous coverage data
echo "Cleaning previous coverage data..."
run_cargo llvm-cov clean --workspace

# Run tests ONCE with coverage instrumentation (no report yet)
echo ""
echo "Running tests with coverage instrumentation..."
run_cargo llvm-cov "${COMMON_OPTS[@]}" \
    --no-report \
    "${TEST_OPTS[@]}"

# Show uncovered lines from collected data (no re-running tests)
echo ""
echo "Showing uncovered lines..."
run_cargo llvm-cov report "${COMMON_OPTS[@]}" \
    --show-missing-lines

echo ""
echo "========================================"
echo "  Uncovered Code Analysis Complete"
echo "========================================"
echo ""

# Get coverage percentage from JSON (no re-running tests)
if command -v jq &> /dev/null; then
    COVERAGE_JSON=$(run_cargo llvm-cov report "${COMMON_OPTS[@]}" --json 2>/dev/null)

    if [ -n "$COVERAGE_JSON" ]; then
        TOTAL_LINES=$(echo "$COVERAGE_JSON" | jq -r '.data[0].totals.lines.count // 0')
        COVERED_LINES=$(echo "$COVERAGE_JSON" | jq -r '.data[0].totals.lines.covered // 0')

        if [ -n "$TOTAL_LINES" ] && [ "$TOTAL_LINES" != "0" ] && [ "$TOTAL_LINES" != "null" ]; then
            # Use awk for floating-point math (more portable than bc)
            PERCENT=$(awk "BEGIN {printf \"%.2f\", $COVERED_LINES * 100 / $TOTAL_LINES}")
            UNCOVERED=$((TOTAL_LINES - COVERED_LINES))

            echo "Line coverage: ${PERCENT}%"
            echo "Covered lines: $COVERED_LINES / $TOTAL_LINES"
            echo "Uncovered lines: $UNCOVERED"
            echo ""

            if [ "$FAIL_MODE" = true ]; then
                # Use awk for floating-point comparison (more portable than bc)
                if awk "BEGIN {exit !($PERCENT < $THRESHOLD)}"; then
                    echo "FAIL: Coverage ${PERCENT}% is below ${THRESHOLD}% threshold"
                    exit 1
                else
                    echo "PASS: Coverage ${PERCENT}% meets ${THRESHOLD}% threshold"
                fi
            fi
        fi
    fi
fi
