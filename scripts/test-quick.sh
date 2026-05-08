#!/usr/bin/env bash
# scripts/test-quick.sh
# Fast feedback loop for development - runs only essential tests
#
# Usage:
#   ./scripts/test-quick.sh           # Run quick tests
#   ./scripts/test-quick.sh --lib     # Only library unit tests
#   ./scripts/test-quick.sh --cli     # Only CLI tests

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-/tmp/rch_target_cass_test_quick}"

# Colors
if [[ -t 1 ]]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    GREEN='' RED='' CYAN='' BOLD='' NC=''
fi

cd "$PROJECT_ROOT"

run_cargo_test() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "ERROR: rch binary not found; quick tests must be offloaded" >&2
        return 127
    fi

    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo test "$@"
}

run_lib_tests() {
    echo -e "${CYAN}Running library unit tests...${NC}"
    run_cargo_test --lib --color=always
}

run_cli_tests() {
    echo -e "${CYAN}Running CLI tests...${NC}"
    run_cargo_test --test e2e_cli_flows --color=always
}

run_connector_tests() {
    echo -e "${CYAN}Running Claude connector tests...${NC}"
    run_cargo_test --test connector_claude --color=always
}

show_help() {
    cat << EOF
Usage: $0 [options]

Fast feedback loop for cass development.

Options:
  --lib       Only run library unit tests
  --cli       Only run CLI e2e tests
  --all       Run lib, cli, and connector tests (default)
  -h, --help  Show this help

EOF
    exit 0
}

# Parse arguments
RUN_LIB=0
RUN_CLI=0
RUN_CONNECTOR=0
RUN_ALL=1

while [[ $# -gt 0 ]]; do
    case $1 in
        --lib)       RUN_LIB=1; RUN_ALL=0; shift ;;
        --cli)       RUN_CLI=1; RUN_ALL=0; shift ;;
        --connector) RUN_CONNECTOR=1; RUN_ALL=0; shift ;;
        -h|--help)   show_help ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

echo -e "${BOLD}Quick Test Runner${NC}"
echo "Using: rch-backed cargo test"
echo "Target dir: $RCH_TARGET_DIR"
echo ""

START_TIME=$(date +%s)
FAILED=0

if [[ $RUN_ALL -eq 1 ]] || [[ $RUN_LIB -eq 1 ]]; then
    run_lib_tests || FAILED=1
fi

if [[ $FAILED -eq 0 ]]; then
    if [[ $RUN_ALL -eq 1 ]] || [[ $RUN_CLI -eq 1 ]]; then
        run_cli_tests || FAILED=1
    fi
fi

if [[ $FAILED -eq 0 ]]; then
    if [[ $RUN_ALL -eq 1 ]] || [[ $RUN_CONNECTOR -eq 1 ]]; then
        run_connector_tests || FAILED=1
    fi
fi

END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))

echo ""
if [[ $FAILED -eq 0 ]]; then
    echo -e "${GREEN}${BOLD}Quick tests passed${NC} (${DURATION}s)"
    exit 0
else
    echo -e "${RED}${BOLD}Quick tests failed${NC} (${DURATION}s)"
    exit 1
fi
