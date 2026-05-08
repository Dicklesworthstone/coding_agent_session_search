#!/usr/bin/env bash
# Coverage generation script for cass
# Generates HTML, LCOV, and JSON coverage reports locally
#
# Usage:
#   ./scripts/coverage.sh           # Generate full coverage report
#   ./scripts/coverage.sh --quick   # Skip HTML generation (faster)
#   ./scripts/coverage.sh --open    # Open HTML report after generation
#
# Environment:
#   RCH_BIN         rch executable (default: rch)
#   RCH_TARGET_DIR  cargo target dir for offloaded coverage work

set -euo pipefail

REPORT_DIR="target/coverage"
QUICK_MODE=false
OPEN_REPORT=false
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_coverage}"

# Parse arguments
for arg in "$@"; do
    case $arg in
        --quick)
            QUICK_MODE=true
            ;;
        --open)
            OPEN_REPORT=true
            ;;
        --help|-h)
            echo "Usage: $0 [--quick] [--open]"
            echo ""
            echo "Options:"
            echo "  --quick    Skip HTML generation (faster)"
            echo "  --open     Open HTML report in browser after generation"
            echo ""
            exit 0
            ;;
    esac
done

# Check dependencies
ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "Error: rch binary not found; coverage Cargo work must be offloaded"
        exit 1
    fi
}

run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

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

if ! command -v jq &> /dev/null; then
    echo "Warning: jq not installed - coverage percentage will not be displayed"
    echo "Install with: brew install jq (macOS) or apt install jq (Linux)"
    echo ""
fi

mkdir -p "$REPORT_DIR"

echo "Generating coverage report..."
echo ""

# Clean previous coverage data
echo "Cleaning previous coverage data..."
run_cargo llvm-cov clean --workspace

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

# Run tests ONCE with coverage instrumentation (no report yet)
echo ""
echo "Running tests with coverage instrumentation..."
run_cargo llvm-cov "${COMMON_OPTS[@]}" \
    --no-report \
    "${TEST_OPTS[@]}"

# Generate reports from collected coverage data (no re-running tests)
echo ""
echo "Generating LCOV report..."
run_cargo llvm-cov report "${COMMON_OPTS[@]}" \
    --lcov \
    --output-path "$REPORT_DIR/lcov.info"

echo "Generating JSON summary..."
run_cargo llvm-cov report "${COMMON_OPTS[@]}" \
    --json \
    --output-path "$REPORT_DIR/coverage.json"

# Generate HTML report (unless quick mode)
if [ "$QUICK_MODE" = false ]; then
    echo "Generating HTML report..."
    run_cargo llvm-cov report "${COMMON_OPTS[@]}" \
        --html \
        --output-dir "$REPORT_DIR/html"
fi

# Print summary to console
echo ""
echo "Coverage Summary"
echo "================"
run_cargo llvm-cov report "${COMMON_OPTS[@]}"

echo ""
echo "Reports generated:"
echo "  LCOV: $REPORT_DIR/lcov.info"
echo "  JSON: $REPORT_DIR/coverage.json"
if [ "$QUICK_MODE" = false ]; then
    echo "  HTML: $REPORT_DIR/html/index.html"
fi

# Extract and display total coverage percentage (requires jq)
if [ -f "$REPORT_DIR/coverage.json" ] && command -v jq &> /dev/null; then
    TOTAL_LINES=$(jq -r '.data[0].totals.lines.count // 0' "$REPORT_DIR/coverage.json" 2>/dev/null || echo "0")
    COVERED_LINES=$(jq -r '.data[0].totals.lines.covered // 0' "$REPORT_DIR/coverage.json" 2>/dev/null || echo "0")
    if [ -n "$TOTAL_LINES" ] && [ "$TOTAL_LINES" != "0" ] && [ "$TOTAL_LINES" != "null" ]; then
        # Use awk for floating-point math (more portable than bc)
        PERCENT=$(awk "BEGIN {printf \"%.2f\", $COVERED_LINES * 100 / $TOTAL_LINES}")
        echo ""
        echo "Total line coverage: ${PERCENT}% ($COVERED_LINES / $TOTAL_LINES lines)"
    fi
fi

# Open HTML report if requested
if [ "$OPEN_REPORT" = true ] && [ "$QUICK_MODE" = false ]; then
    HTML_PATH="$REPORT_DIR/html/index.html"
    if [ -f "$HTML_PATH" ]; then
        echo ""
        echo "Opening coverage report in browser..."
        if command -v open &> /dev/null; then
            open "$HTML_PATH"  # macOS
        elif command -v xdg-open &> /dev/null; then
            xdg-open "$HTML_PATH"  # Linux
        else
            echo "Could not detect browser opener. Open manually: $HTML_PATH"
        fi
    fi
fi

echo ""
echo "Done!"
