#!/usr/bin/env bash
# run_e2e_smoke.sh — TUI e2e smoke runner.
#
# Per coding_agent_session_search-8m208. Runs the e2e_scenario_* tests in
# src/ui/app.rs::tests in serial under --nocapture, captures wall time,
# and emits a structured summary. On failure, dumps the captured stderr
# plus a reproduction command.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-/tmp/cass-smoke-target}"
LOG="$RCH_TARGET_DIR/e2e-smoke.log"
mkdir -p "$RCH_TARGET_DIR"
exec > >(tee -a "$LOG") 2>&1

cleanup() {
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        echo ""
        echo "[e2e_smoke] FAILURE — last 50 log lines:" >&2
        tail -n 50 "$LOG" | sed 's/^/[e2e_smoke]   /' >&2
        echo "[e2e_smoke] Reproduce a single failing scenario via:" >&2
        echo "[e2e_smoke]   rch exec -- env CARGO_TARGET_DIR=$RCH_TARGET_DIR \\" >&2
        echo "[e2e_smoke]     cargo test --lib e2e_scenario_<name> -- --nocapture --test-threads=1" >&2
    fi
    exit "$rc"
}
trap cleanup EXIT

# Discover the scenario list from src/ui/app.rs's test module. This avoids
# hardcoding scenario names and stays current as scenarios are added.
SCENARIOS=$(grep -E "^\s*fn e2e_scenario_[a-z_]+" "$PROJECT_ROOT/src/ui/app.rs" \
    | sed -E 's/^\s*fn (e2e_scenario_[a-z_]+).*/\1/' \
    | sort -u)

if [ -z "$SCENARIOS" ]; then
    echo "[e2e_smoke] No e2e_scenario_* tests found in src/ui/app.rs"
    exit 0
fi

PASS=0
FAIL=0
TOTAL=0
START_TS="$(date +%s)"
for sc in $SCENARIOS; do
    TOTAL=$((TOTAL + 1))
    sc_start="$(date +%s%N)"
    if rch exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" \
        cargo test --lib "$sc" -- --nocapture --test-threads=1 \
        >> "$LOG" 2>&1; then
        sc_elapsed_ms=$(( ($(date +%s%N) - sc_start) / 1000000 ))
        echo "[e2e_smoke] OK: $sc (${sc_elapsed_ms}ms)"
        PASS=$((PASS + 1))
    else
        sc_elapsed_ms=$(( ($(date +%s%N) - sc_start) / 1000000 ))
        echo "[e2e_smoke] FAIL: $sc (${sc_elapsed_ms}ms)"
        FAIL=$((FAIL + 1))
    fi
done

ELAPSED_S=$(( $(date +%s) - START_TS ))
echo ""
echo "[e2e_smoke] e2e_smoke: TOTAL=$TOTAL PASS=$PASS FAIL=$FAIL WALL_S=$ELAPSED_S"
echo "[e2e_smoke] log: $LOG"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
echo "[e2e_smoke] ALL PASS"
