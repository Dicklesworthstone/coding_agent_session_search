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
RUN_INTEGRATION=1
RUN_GOLDENS=1
LOCAL=0
REGEN_GOLDENS=0
RUN_DOCS_TRUTH=0
# Each entry is `<test-binary>` or `<test-binary>:<filter words>`; the filter
# is passed after `--` so one admission can run only the tests a change touches.
INTEGRATION_TESTS=(cli_robot bookmarks_cli)
while [ $# -gt 0 ]; do
    case "$1" in
        --lib-filter) LIB_FILTER="$2"; shift 2 ;;
        --no-lib) RUN_LIB=0; shift ;;
        --local) LOCAL=1; shift ;;
        --integration) IFS=',' read -r -a INTEGRATION_TESTS <<<"$2"; shift 2 ;;
        # Regenerate goldens (UPDATE_GOLDENS=1) before verifying them. The
        # verify stage still runs, and every resulting diff under tests/golden
        # must be read before it is committed — regeneration is never a way to
        # make a red golden green.
        --regen-goldens) REGEN_GOLDENS=1; shift ;;
        # The fleet closes an SSH session after 30 minutes (rch E104). A full
        # lib suite plus integration plus goldens does not fit in one job on a
        # busy worker, so split: `--lib-only` (fmt, clippy, lib suite) and a
        # second run with `--no-lib` for integration + goldens.
        --lib-only) RUN_INTEGRATION=0; RUN_GOLDENS=0; shift ;;
        --no-goldens) RUN_GOLDENS=0; shift ;;
        # README ↔ code truth (WS-A.9): key bindings, `cass … --flag` usages
        # and env vars, checked with the debug binary the integration stage
        # just built. Opt-in until the README is clean against main.
        --docs-truth) RUN_DOCS_TRUTH=1; shift ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# The fleet's warm target dir. Deliberately NOT the ambient CARGO_TARGET_DIR:
# that variable points at a local build dir in many agent shells, and passing
# it through made a gate run compile clippy from scratch on the worker
# (13 minutes instead of 5). Override with GATE_TARGET_DIR when needed.
TARGET_DIR="${GATE_TARGET_DIR:-/data/tmp/cass-check-target}"
RETRIES="${GATE_RETRIES:-1}"
RETRY_SLEEP="${GATE_RETRY_SLEEP:-90}"

# The remote script. Every stage records its own exit code; `tail` keeps the
# transcript bounded so the receipt lines stay readable in a terminal.
lib_stage=""
if [ "$RUN_LIB" = 1 ]; then
    # A wedged test (the bet45 class: a producer parked forever at ~0 CPU)
    # would otherwise hold the fleet admission until the job's own ceiling.
    # `timeout` turns that into a loud EXIT=124 on this stage instead.
    LIB_TIMEOUT="${GATE_LIB_TIMEOUT_SECS:-2400}"
    lib_stage="timeout ${LIB_TIMEOUT} cargo test --lib -- ${LIB_FILTER} 2>&1 | tail -80; echo STAGE=lib-tests EXIT=\${PIPESTATUS[0]};"
fi
# Every stage the remote script will run, in order. The receipt check below
# requires each one to report: a job cut short by the fleet's SSH ceiling
# leaves later stages missing, and a missing stage is RED, never green.
EXPECTED_STAGES=(fmt clippy)
[ "$RUN_LIB" = 1 ] && EXPECTED_STAGES+=(lib-tests)
integration_stage=""
if [ "$RUN_INTEGRATION" = 1 ]; then
    for entry in "${INTEGRATION_TESTS[@]}"; do
        [ -n "$entry" ] || continue
        t="${entry%%:*}"
        filter=""
        if [ "$entry" != "$t" ]; then
            filter="${entry#*:}"
        fi
        integration_stage+="cargo test --test ${t} -- ${filter} 2>&1 | tail -40; echo STAGE=test-${t} EXIT=\${PIPESTATUS[0]};"
        EXPECTED_STAGES+=("test-${t}")
    done
fi
golden_regen_stage=""
golden_stage=""
if [ "$RUN_GOLDENS" = 1 ]; then
    if [ "$REGEN_GOLDENS" = 1 ]; then
        golden_regen_stage="UPDATE_GOLDENS=1 cargo test --test golden_robot_json --test golden_robot_docs 2>&1 | tail -30; echo STAGE=goldens-regen EXIT=\${PIPESTATUS[0]};"
        EXPECTED_STAGES+=(goldens-regen)
    fi
    golden_stage="cargo test --test golden_robot_json --test golden_robot_docs 2>&1 | tail -30; echo STAGE=goldens EXIT=\${PIPESTATUS[0]};"
    EXPECTED_STAGES+=(goldens)
fi

docs_truth_stage=""
if [ "$RUN_DOCS_TRUTH" = 1 ]; then
    # The integration stage leaves a debug `cass` in the target dir; the
    # validator needs it for `cass … --help` and `robot-docs env`.
    docs_truth_stage="CASS_BIN=${TARGET_DIR}/debug/cass scripts/validate_docs.sh --truth 2>&1 | tail -60; echo STAGE=docs-truth EXIT=\${PIPESTATUS[0]};"
    EXPECTED_STAGES+=(docs-truth)
fi

REMOTE_SCRIPT="cargo fmt --check 2>&1 | tail -20; echo STAGE=fmt EXIT=\${PIPESTATUS[0]}; \
cargo clippy --all-targets -- -D warnings 2>&1 | tail -40; echo STAGE=clippy EXIT=\${PIPESTATUS[0]}; \
${lib_stage} ${integration_stage} ${docs_truth_stage} ${golden_regen_stage} ${golden_stage} echo STAGE=job-complete EXIT=0"
EXPECTED_STAGES+=(job-complete)

run_once() {
    if [ "$LOCAL" = 1 ]; then
        env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=16777216 bash -c "$REMOTE_SCRIPT"
        return $?
    fi
    rch exec --job --result-dir tests/golden -- env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=16777216 bash -c "$REMOTE_SCRIPT"
}

# The receipt survives a RED run for post-mortem (GATE_RECEIPT_FILE overrides).
receipt_file="${GATE_RECEIPT_FILE:-$(mktemp -t cass-gate.XXXXXX)}"
: > "$receipt_file"
attempt=1
rc=103
while [ "$attempt" -le "$RETRIES" ]; do
    echo "gate attempt ${attempt}/${RETRIES} $(date +%T)" >&2
    # Both streams: rch relays remote output on whichever stream it chooses,
    # and a receipt that misses the STAGE lines reads as a truncated job.
    run_once 2>&1 | tee "$receipt_file"
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

echo "---- gate receipt $(date -u +%Y-%m-%dT%H:%M:%SZ) HEAD=$(git rev-parse --short HEAD) rch_exit=${rc} receipt=${receipt_file} lines=$(wc -l < "$receipt_file") ----"
failed=0
# The remote job's own exit is part of the receipt: an SSH ceiling (E104), a
# worker loss, or a sync failure must never read as green because the stages
# that did run happened to pass.
if [ "$rc" -ne 0 ]; then
    echo "STAGE=rch EXIT=${rc} (remote job did not complete normally)"
    failed=1
fi
declare -A seen_stage=()
while IFS= read -r line; do
    echo "$line"
    name="${line#STAGE=}"; name="${name%% *}"
    seen_stage["$name"]=1
    case "$line" in
        *" EXIT=0") ;;
        *) failed=1 ;;
    esac
done < <(grep -E '^STAGE=[a-z_-]+ EXIT=[0-9]+$' "$receipt_file")
for stage in "${EXPECTED_STAGES[@]}"; do
    if [ -z "${seen_stage[$stage]:-}" ]; then
        echo "STAGE=${stage} EXIT=missing (no receipt: job cut short?)"
        failed=1
    fi
done
if [ "$failed" -ne 0 ]; then
    echo "gate: RED (receipt kept at ${receipt_file})" >&2
    exit 1
fi
rm -f "$receipt_file"
echo "gate: GREEN"
