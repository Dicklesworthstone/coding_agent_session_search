#!/usr/bin/env bash
# test-report.sh - Run tests with JUnit XML and optional HTML report
#
# Usage:
#   ./scripts/test-report.sh           # Run all tests with JUnit output
#   ./scripts/test-report.sh --quick   # Run unit tests only (skip E2E)
#   ./scripts/test-report.sh --e2e     # Run E2E tests only
#   ./scripts/test-report.sh --open    # Open HTML report after generation
#
# Reports are generated in ${RCH_TARGET_DIR}/nextest/<profile>/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_test_report}"

QUICK_MODE=false
E2E_ONLY=false
OPEN_REPORT=false
PROFILE="ci"

# Parse arguments
for arg in "$@"; do
    case $arg in
        --quick)
            QUICK_MODE=true
            ;;
        --e2e)
            E2E_ONLY=true
            PROFILE="e2e"
            ;;
        --open)
            OPEN_REPORT=true
            ;;
        --help|-h)
            echo "Usage: $0 [--quick] [--e2e] [--open]"
            echo ""
            echo "Options:"
            echo "  --quick    Run unit tests only (skip E2E tests)"
            echo "  --e2e      Run E2E tests only (uses e2e profile)"
            echo "  --open     Open HTML report in browser (requires junit2html)"
            echo ""
            echo "Profiles:"
            echo "  ci   - Full test suite with JUnit XML output"
            echo "  e2e  - E2E tests with sequential execution"
            echo ""
            echo "Environment:"
            echo "  RCH_BIN         rch executable (default: rch)"
            echo "  RCH_TARGET_DIR  cargo target dir for offloaded tests (default: \${TMPDIR:-/tmp}/rch_target_cass_test_report)"
            echo ""
            echo "Reports generated in: \$RCH_TARGET_DIR/nextest/<profile>/junit.xml"
            echo ""
            exit 0
            ;;
    esac
done

cd "$PROJECT_ROOT"

ensure_rch() {
    if ! command -v "$RCH_BIN" &> /dev/null; then
        echo "Error: rch binary not found; test-report cargo work must be offloaded"
        exit 1
    fi
}

run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

ensure_rch

# Check if cargo-nextest is available through the offloaded cargo toolchain.
if ! run_cargo nextest --version > /dev/null 2>&1; then
    echo "Error: cargo-nextest is unavailable through rch"
    echo ""
    echo "Install it on the rch worker toolchain, or use a worker image that includes cargo-nextest."
    echo ""
    exit 1
fi

echo "==================================="
echo "  cass Test Report Generator"
echo "==================================="
echo ""

# Build first
echo "Building project..."
run_cargo build --tests --quiet

# Determine filter expression
FILTER_ARGS=()
if [ "$E2E_ONLY" = true ]; then
    FILTER_ARGS=(-E "binary(install_scripts) | binary(e2e_index_tui) | binary(e2e_filters) | binary(e2e_multi_connector)")
    echo "Running: E2E tests only"
elif [ "$QUICK_MODE" = true ]; then
    FILTER_ARGS=(-E "not (test(install_sh) | test(install_ps1) | binary(~e2e) | binary(install_scripts))")
    echo "Running: Unit tests (skipping E2E and install script tests)"
else
    FILTER_ARGS=(-E "not (test(install_sh) | test(install_ps1))")
    echo "Running: All tests (skipping install script tests)"
fi

echo "Profile: $PROFILE"
echo "Target dir: $RCH_TARGET_DIR"
echo ""

# Run tests
echo "Running tests..."
echo "-----------------------------------"

run_cargo nextest run --profile "$PROFILE" "${FILTER_ARGS[@]}" --no-fail-fast 2>&1 || true

echo ""
echo "-----------------------------------"
echo "Test Report"
echo "-----------------------------------"

JUNIT_PATH="$RCH_TARGET_DIR/nextest/$PROFILE/junit.xml"

if [ -f "$JUNIT_PATH" ]; then
    echo "JUnit XML report: $JUNIT_PATH"

    # Parse basic stats from JUnit XML
    if command -v xmllint &> /dev/null; then
        # Use /testsuites for aggregate stats (nextest puts totals on root element)
        TESTS=$(xmllint --xpath 'string(/testsuites/@tests)' "$JUNIT_PATH" 2>/dev/null || echo "?")
        FAILURES=$(xmllint --xpath 'string(/testsuites/@failures)' "$JUNIT_PATH" 2>/dev/null || echo "?")
        ERRORS=$(xmllint --xpath 'string(/testsuites/@errors)' "$JUNIT_PATH" 2>/dev/null || echo "?")
        TIME=$(xmllint --xpath 'string(/testsuites/@time)' "$JUNIT_PATH" 2>/dev/null || echo "?")

        echo ""
        echo "Summary:"
        echo "  Tests:    $TESTS"
        echo "  Failures: $FAILURES"
        echo "  Errors:   $ERRORS"
        echo "  Time:     ${TIME}s"
    fi

    # Generate HTML report if junit2html is available
    if [ "$OPEN_REPORT" = true ]; then
        if command -v junit2html &> /dev/null; then
            HTML_PATH="$RCH_TARGET_DIR/nextest/$PROFILE/report.html"
            echo ""
            echo "Generating HTML report..."
            junit2html "$JUNIT_PATH" "$HTML_PATH"
            echo "HTML report: $HTML_PATH"

            # Open in browser
            if command -v open &> /dev/null; then
                open "$HTML_PATH"  # macOS
            elif command -v xdg-open &> /dev/null; then
                xdg-open "$HTML_PATH"  # Linux
            else
                echo "Could not detect browser opener. Open manually: $HTML_PATH"
            fi
        else
            echo ""
            echo "Note: Install junit2html for HTML reports: pip install junit2html"
            echo "Opening raw XML file..."
            if command -v open &> /dev/null; then
                open "$JUNIT_PATH"  # macOS
            elif command -v xdg-open &> /dev/null; then
                xdg-open "$JUNIT_PATH"  # Linux
            fi
        fi
    fi
else
    echo "Warning: JUnit XML report not found at $JUNIT_PATH"
    echo "Tests may have failed to run."
fi

echo ""
echo "Done!"
