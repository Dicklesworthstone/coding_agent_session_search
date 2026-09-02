#!/usr/bin/env bash
# gate.sh — the blocking quality gate, batched into ONE rch fleet admission.
#
# Why this exists (reality check 2026-09-01, WS-A.2): every GitHub workflow is
# disabled, so the only gate is agent-run. Fleet admissions are scarce, and an
# agent that spends one on `cargo check`, then another on clippy, then another
# on tests loses most of an hour to refusals — and the temptation to push
# unverified follows. This script runs exactly what CI will run, inside a
# single remote job, and prints one receipt line per stage:
#
#   STAGE=<name> EXIT=<code>
#
# The receipt is what a bead closure or a push cites. Nothing here weakens a
# gate: stages run with the same flags CI uses, and a stage failure never stops
# the later stages (you want the whole picture from one admission).
#
# Usage:
#   scripts/gate.sh                 # fmt, clippy, lib tests, targeted integration, goldens
#   scripts/gate.sh --lib-filter 'quill_bridge::tests health_watermark'   # narrow the lib run
#   scripts/gate.sh --no-lib        # skip the lib suite (e.g. while bet45 wedge is open)
#   scripts/gate.sh --local         # run stages locally (only where rch is not required)
#   GATE_RETRIES=40 GATE_RETRY_SLEEP=90 scripts/gate.sh   # keep retrying fleet refusals
#
# Exit code: 0 when every stage exited 0, 1 otherwise, 103 if the fleet refused
# every attempt (RCH_REQUIRE_REMOTE=1 forbids local fallback).

set -uo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_ROOT"

LIB_FILTER=""
RUN_LIB=1
LOCAL=0
INTEGRATION_TESTS=(cli_robot bookmarks_cli)
while [ $# -gt 0 ]; do
    case "$1" in
        --lib-filter) LIB_FILTER="$2"; shift 2 ;;
        --no-lib) RUN_LIB=0; shift ;;
        --local) LOCAL=1; shift ;;
        --integration) IFS=',' read -r -a INTEGRATION_TESTS <<<"$2"; shift 2 ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cass-check-target}"
RETRIES="${GATE_RETRIES:-1}"
RETRY_SLEEP="${GATE_RETRY_SLEEP:-90}"

# The remote script. Every stage records its own exit code; `tail` keeps the
# transcript bounded so the receipt lines stay readable in a terminal.
lib_stage=""
if [ "$RUN_LIB" = 1 ]; then
    lib_stage="cargo test --lib -- ${LIB_FILTER} 2>&1 | tail -80; echo STAGE=lib-tests EXIT=\${PIPESTATUS[0]};"
fi
integration_stage=""
for t in "${INTEGRATION_TESTS[@]}"; do
    integration_stage+="cargo test --test ${t} 2>&1 | tail -30; echo STAGE=test-${t} EXIT=\${PIPESTATUS[0]};"
done

REMOTE_SCRIPT="cargo fmt --check 2>&1 | tail -20; echo STAGE=fmt EXIT=\${PIPESTATUS[0]}; \
cargo clippy --all-targets -- -D warnings 2>&1 | tail -40; echo STAGE=clippy EXIT=\${PIPESTATUS[0]}; \
${lib_stage} ${integration_stage} \
cargo test --test golden_robot_json --test golden_robot_docs 2>&1 | tail -30; echo STAGE=goldens EXIT=\${PIPESTATUS[0]}"

run_once() {
    if [ "$LOCAL" = 1 ]; then
        env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=16777216 bash -c "$REMOTE_SCRIPT"
        return $?
    fi
    rch exec --job --result-dir tests/golden -- env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=16777216 bash -c "$REMOTE_SCRIPT"
}

receipt_file="$(mktemp -t cass-gate.XXXXXX)"
attempt=1
rc=103
while [ "$attempt" -le "$RETRIES" ]; do
    echo "gate attempt ${attempt}/${RETRIES} $(date +%T)" >&2
    run_once | tee "$receipt_file"
    rc=${PIPESTATUS[0]}
    if [ "$rc" -ne 103 ]; then
        break
    fi
    attempt=$((attempt + 1))
    [ "$attempt" -le "$RETRIES" ] && sleep "$RETRY_SLEEP"
done

if [ "$rc" -eq 103 ]; then
    echo "gate: fleet refused every attempt (exit 103); nothing was verified" >&2
    rm -f "$receipt_file"
    exit 103
fi

echo "---- gate receipt $(date -u +%Y-%m-%dT%H:%M:%SZ) HEAD=$(git rev-parse --short HEAD) ----"
failed=0
while IFS= read -r line; do
    echo "$line"
    case "$line" in
        *" EXIT=0") ;;
        *) failed=1 ;;
    esac
done < <(grep -E '^STAGE=[a-z_-]+ EXIT=[0-9]+$' "$receipt_file")
rm -f "$receipt_file"
if [ "$failed" -ne 0 ]; then
    echo "gate: RED" >&2
    exit 1
fi
echo "gate: GREEN"
