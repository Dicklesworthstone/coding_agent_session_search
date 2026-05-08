#!/usr/bin/env bash
# scripts/daemon/cass_daemon_e2e.sh
# End-to-end daemon fallback flow with structured JSONL logs and JSON report.
#
# This script tests the daemon warm embedder/reranker fallback behavior:
# - Validates fallback to local embedder/reranker paths when daemon unavailable
# - Emits structured JSONL logs with phase markers per E2E logging schema
# - Exercises daemon unavailable scenario (no daemon server running)
#
# Output files:
# - target/e2e-daemon/run_<id>/daemon_e2e.jsonl  (JSONL events)
# - target/e2e-daemon/run_<id>/report.json       (Final report)
# - target/e2e-daemon/run_<id>/run.log           (Human-readable log)
#
# Environment:
# - RCH_BIN         rch executable (default: rch)
# - RCH_TARGET_DIR  cargo target dir for offloaded cass build

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_daemon_e2e}"

# Source standard E2E logging library (emits to test-results/e2e/)
# shellcheck disable=SC1091
source "${PROJECT_ROOT}/scripts/lib/e2e_log.sh"
e2e_init "shell" "daemon_fallback"

RUN_ID="$(date +"%Y%m%d_%H%M%S")_${RANDOM}"
LOG_ROOT="${PROJECT_ROOT}/target/e2e-daemon"
RUN_DIR="${LOG_ROOT}/run_${RUN_ID}"
LOG_FILE="${RUN_DIR}/run.log"
JSONL_FILE="${RUN_DIR}/daemon_e2e.jsonl"
REPORT_JSON="${RUN_DIR}/report.json"
STDOUT_DIR="${RUN_DIR}/stdout"
STDERR_DIR="${RUN_DIR}/stderr"

SANDBOX_DIR="${RUN_DIR}/sandbox"
DATA_DIR="${SANDBOX_DIR}/cass_data"
CODEX_HOME="${SANDBOX_DIR}/.codex"
HOME_DIR="${SANDBOX_DIR}/home"

NO_BUILD=0
EMBEDDER="hash"
QUERY="binary search"
HEALTH_CHECK=1

DAEMON_RETRY_MAX="${CASS_DAEMON_RETRY_MAX:-2}"
DAEMON_BACKOFF_BASE_MS="${CASS_DAEMON_BACKOFF_BASE_MS:-200}"
DAEMON_BACKOFF_MAX_MS="${CASS_DAEMON_BACKOFF_MAX_MS:-5000}"
DAEMON_JITTER_PCT="${CASS_DAEMON_JITTER_PCT:-0.2}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)
            NO_BUILD=1
            shift
            ;;
        --embedder)
            shift
            if [[ $# -gt 0 ]]; then
                EMBEDDER="$1"
                shift
            fi
            ;;
        --query)
            shift
            if [[ $# -gt 0 ]]; then
                QUERY="$1"
                shift
            fi
            ;;
        --skip-health-check)
            HEALTH_CHECK=0
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [--no-build] [--embedder hash|fastembed] [--query \"text\"] [--skip-health-check]"
            echo ""
            echo "Options:"
            echo "  --no-build           Skip cargo build step"
            echo "  --embedder MODEL     Use 'hash' or 'fastembed' embedder (default: hash)"
            echo "  --query TEXT         Search query to test (default: 'binary search')"
            echo "  --skip-health-check  Skip health/status validation"
            exit 0
            ;;
        *)
            shift
            ;;
    esac
done

# shellcheck disable=SC2034
if [[ -t 1 ]]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    CYAN='\033[0;36m'
    YELLOW='\033[0;33m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    GREEN='' RED='' CYAN='' YELLOW='' BOLD='' NC=''
fi

mkdir -p "${RUN_DIR}" "${STDOUT_DIR}" "${STDERR_DIR}" "${SANDBOX_DIR}" "${DATA_DIR}" "${CODEX_HOME}" "${HOME_DIR}"

# Track test results for summary
TESTS_TOTAL=0
TESTS_PASSED=0
TESTS_FAILED=0
RUN_START_MS=0
declare -A PHASE_STARTS

log() {
    local level=$1
    shift
    local msg="$*"
    local ts
    ts=$(date +"%Y-%m-%d %H:%M:%S.%3N" 2>/dev/null || date +"%Y-%m-%d %H:%M:%S")
    echo "[${ts}] [${level}] ${msg}" | tee -a "${LOG_FILE}"
}

json_escape() {
    local s="$1"
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//$'\n'/\\n}
    s=${s//$'\r'/\\r}
    s=${s//$'\t'/\\t}
    printf '%s' "$s"
}

now_ms() {
    if date +%s%3N >/dev/null 2>&1; then
        date +%s%3N
        return
    fi
    if command -v python3 >/dev/null 2>&1; then
        python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
        return
    fi
    echo "$(( $(date +%s) * 1000 ))"
}

iso_timestamp() {
    date -u +"%Y-%m-%dT%H:%M:%S.000Z"
}

# =============================================================================
# JSONL Event Emission Functions (E2E logging schema)
# =============================================================================

emit_jsonl() {
    echo "$1" >> "${JSONL_FILE}"
}

emit_run_start() {
    RUN_START_MS=$(now_ms)
    local git_sha git_branch os arch cass_version ci
    git_sha=$(git -C "${PROJECT_ROOT}" rev-parse --short HEAD 2>/dev/null || echo "unknown")
    git_branch=$(git -C "${PROJECT_ROOT}" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
    os=$(uname -s | tr '[:upper:]' '[:lower:]')
    arch=$(uname -m)
    cass_version="${CASS_VERSION:-0.1.61}"
    ci="${CI:-false}"

    emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"run_start\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"env\":{\"git_sha\":\"${git_sha}\",\"git_branch\":\"${git_branch}\",\"os\":\"${os}\",\"arch\":\"${arch}\",\"cass_version\":\"${cass_version}\",\"ci\":${ci}}}"
}

emit_phase_start() {
    local phase_name=$1
    local description=${2:-""}
    PHASE_STARTS["${phase_name}"]=$(now_ms)
    emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"phase_start\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"phase\":{\"name\":\"${phase_name}\",\"description\":\"$(json_escape "$description")\"}}"
}

emit_phase_end() {
    local phase_name=$1
    local start_ms=${PHASE_STARTS["${phase_name}"]:-$(now_ms)}
    local end_ms
    end_ms=$(now_ms)
    local duration_ms=$((end_ms - start_ms))
    emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"phase_end\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"phase\":{\"name\":\"${phase_name}\"},\"duration_ms\":${duration_ms}}"
}

emit_test_start() {
    local test_name=$1
    local suite=${2:-"daemon"}
    TESTS_TOTAL=$((TESTS_TOTAL + 1))
    emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"test_start\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"test\":{\"name\":\"${test_name}\",\"suite\":\"${suite}\",\"file\":\"scripts/daemon/cass_daemon_e2e.sh\"}}"
}

emit_test_end() {
    local test_name=$1
    local status=$2
    local duration_ms=$3
    local error=${4:-""}

    if [[ "${status}" == "pass" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"test_end\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"test\":{\"name\":\"${test_name}\"},\"result\":{\"status\":\"pass\",\"duration_ms\":${duration_ms}}}"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"test_end\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"test\":{\"name\":\"${test_name}\"},\"result\":{\"status\":\"fail\",\"duration_ms\":${duration_ms},\"error\":\"$(json_escape "$error")\"}}"
    fi
}

emit_metrics() {
    local name=$1
    shift
    # Remaining args are key=value pairs
    local metrics="{"
    local first=1
    for kv in "$@"; do
        local key="${kv%%=*}"
        local value="${kv#*=}"
        if [[ $first -eq 0 ]]; then
            metrics+=","
        fi
        metrics+="\"${key}\":${value}"
        first=0
    done
    metrics+="}"
    emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"metrics\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"name\":\"${name}\",\"metrics\":${metrics}}"
}

emit_run_end() {
    local exit_code=$1
    local end_ms
    end_ms=$(now_ms)
    local duration_ms=$((end_ms - RUN_START_MS))
    emit_jsonl "{\"ts\":\"$(iso_timestamp)\",\"event\":\"run_end\",\"run_id\":\"${RUN_ID}\",\"runner\":\"bash\",\"summary\":{\"total\":${TESTS_TOTAL},\"passed\":${TESTS_PASSED},\"failed\":${TESTS_FAILED},\"skipped\":0,\"duration_ms\":${duration_ms}},\"exit_code\":${exit_code}}"
}

run_step() {
    local name=$1
    shift
    local stdout_file="${STDOUT_DIR}/${name}.out"
    local stderr_file="${STDERR_DIR}/${name}.err"
    local exit_code

    log "STEP" "${name}: $*"
    set +e
    "$@" >"${stdout_file}" 2>"${stderr_file}"
    exit_code=$?
    set -e

    if [[ $exit_code -eq 0 ]]; then
        log "OK" "${name}"
    else
        log "FAIL" "${name} (exit ${exit_code})"
    fi
    return "$exit_code"
}

# shellcheck disable=SC2317
ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        log "FAIL" "rch binary not found; daemon E2E cass build must be offloaded"
        emit_run_end 1
        e2e_run_end 0 0 0 0 "$(e2e_duration_since_start)"
        exit 1
    fi
}

# shellcheck disable=SC2317
run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

# shellcheck disable=SC2317
build_cass_binary() {
    ensure_rch
    (cd "$PROJECT_ROOT" && run_cargo build)
}

# =============================================================================
# Main E2E Flow
# =============================================================================

log "INFO" "Run directory: ${RUN_DIR}"
log "INFO" "JSONL output: ${JSONL_FILE}"

# Initialize JSONL file
: > "${JSONL_FILE}"

# Emit run_start event
emit_run_start
e2e_run_start

# Phase: Build (if enabled)
if [[ $NO_BUILD -eq 0 ]]; then
    emit_phase_start "build" "Compile cass binary"
    e2e_phase_start "build" "Compile cass binary"
    run_step "build" build_cass_binary
    emit_phase_end "build"
    e2e_phase_end "build" 0
fi

if [[ -z "${CASS_BIN:-}" ]]; then
    if [[ $NO_BUILD -eq 0 ]]; then
        CASS_BIN="${RCH_TARGET_DIR}/debug/cass"
    else
        CASS_BIN="${PROJECT_ROOT}/target/debug/cass"
    fi
fi

if [[ ! -x "$CASS_BIN" ]]; then
    log "FAIL" "cass binary not found or not executable at ${CASS_BIN}"
    emit_run_end 1
    e2e_run_end 0 0 0 0 "$(e2e_duration_since_start)"
    exit 1
fi

run_step "version" "$CASS_BIN" --version

# Phase: Setup sandbox data
emit_phase_start "setup_sandbox" "Prepare test fixtures"
e2e_phase_start "setup_sandbox" "Prepare test fixtures"
log "INFO" "Preparing sandbox data"
mkdir -p "${CODEX_HOME}/sessions/2024/11/20"
cat > "${CODEX_HOME}/sessions/2024/11/20/rollout-daemon-e2e.jsonl" <<'JSONL'
{"type":"event_msg","timestamp":1732118400000,"payload":{"type":"user_message","message":"Explain daemon fallback behavior"}}
{"type":"response_item","timestamp":1732118401000,"payload":{"role":"assistant","content":"Daemon fallback should be transparent to users."}}
{"type":"event_msg","timestamp":1732118402000,"payload":{"type":"user_message","message":"Add retry logic with jittered backoff"}}
{"type":"response_item","timestamp":1732118403000,"payload":{"role":"assistant","content":"Retries should include randomized jitter to avoid thundering herd."}}
JSONL
emit_phase_end "setup_sandbox"
e2e_phase_end "setup_sandbox" 0

export CASS_DATA_DIR="${DATA_DIR}"
export CODEX_HOME="${CODEX_HOME}"
export HOME="${HOME_DIR}"
export CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1

pushd "${SANDBOX_DIR}" >/dev/null

# Phase: Indexing
emit_phase_start "indexing" "Build full-text and semantic indexes"
e2e_phase_start "indexing" "Build full-text and semantic indexes"
run_step "index_full" "$CASS_BIN" index --full --data-dir "${DATA_DIR}"
run_step "index_semantic" "$CASS_BIN" index --semantic --full --embedder "${EMBEDDER}" --data-dir "${DATA_DIR}"
emit_phase_end "indexing"
e2e_phase_end "indexing" 0

# =============================================================================
# Test: Health/Status Check
# =============================================================================
if [[ $HEALTH_CHECK -eq 1 ]]; then
    emit_phase_start "health_check" "Validate cass status command"
    e2e_phase_start "health_check" "Validate cass status command"
    emit_test_start "test_status_command"
    e2e_test_start "test_status_command" "daemon"
    TEST_START_MS=$(now_ms)

    STATUS_STDOUT="${STDOUT_DIR}/status.out"
    STATUS_STDERR="${STDERR_DIR}/status.err"
    set +e
    "$CASS_BIN" status --json --data-dir "${DATA_DIR}" >"${STATUS_STDOUT}" 2>"${STATUS_STDERR}"
    STATUS_EXIT=$?
    set -e

    TEST_END_MS=$(now_ms)
    TEST_DURATION_MS=$((TEST_END_MS - TEST_START_MS))

    if [[ $STATUS_EXIT -eq 0 ]]; then
        log "OK" "status command"
        emit_test_end "test_status_command" "pass" "$TEST_DURATION_MS"
        e2e_test_pass "test_status_command" "daemon" "$TEST_DURATION_MS"
    else
        log "FAIL" "status command (exit ${STATUS_EXIT})"
        emit_test_end "test_status_command" "fail" "$TEST_DURATION_MS" "status command returned exit code ${STATUS_EXIT}"
        e2e_test_fail "test_status_command" "daemon" "$TEST_DURATION_MS" 0 "exit code ${STATUS_EXIT}" "CommandFailed"
    fi
    emit_phase_end "health_check"
    e2e_phase_end "health_check" "$TEST_DURATION_MS"
fi

# =============================================================================
# Test: Daemon Fallback (unavailable scenario)
# =============================================================================
emit_phase_start "daemon_fallback_test" "Test daemon fallback when unavailable"
e2e_phase_start "daemon_fallback_test" "Test daemon fallback when unavailable"
emit_test_start "test_daemon_fallback_unavailable"
e2e_test_start "test_daemon_fallback_unavailable" "daemon"

SEARCH_MODEL_FLAGS=()
if [[ "${EMBEDDER}" == "hash" ]]; then
    SEARCH_MODEL_FLAGS=(--model hash)
fi

SEARCH_STDOUT="${STDOUT_DIR}/search.out"
SEARCH_STDERR="${STDERR_DIR}/search.err"
SEARCH_START_MS=$(now_ms)
set +e
"$CASS_BIN" --verbose search "${QUERY}" \
    --mode semantic \
    --daemon \
    --json \
    --data-dir "${DATA_DIR}" \
    "${SEARCH_MODEL_FLAGS[@]}" \
    >"${SEARCH_STDOUT}" 2>"${SEARCH_STDERR}"
SEARCH_EXIT=$?
set -e
SEARCH_END_MS=$(now_ms)
SEARCH_LATENCY_MS=$((SEARCH_END_MS - SEARCH_START_MS))

if [[ $SEARCH_EXIT -eq 0 ]]; then
    log "OK" "search"
    emit_test_end "test_daemon_fallback_unavailable" "pass" "$SEARCH_LATENCY_MS"
    e2e_test_pass "test_daemon_fallback_unavailable" "daemon" "$SEARCH_LATENCY_MS"
else
    log "FAIL" "search (exit ${SEARCH_EXIT})"
    emit_test_end "test_daemon_fallback_unavailable" "fail" "$SEARCH_LATENCY_MS" "search with daemon flag returned exit code ${SEARCH_EXIT}"
    e2e_test_fail "test_daemon_fallback_unavailable" "daemon" "$SEARCH_LATENCY_MS" 0 "exit code ${SEARCH_EXIT}" "CommandFailed"
fi

# Parse fallback metrics from stderr
ATTEMPT_EMBED=$(grep -c "Attempting daemon embed$" "${SEARCH_STDERR}" || true)
ATTEMPT_RERANK=$(grep -c "Attempting daemon rerank$" "${SEARCH_STDERR}" || true)
FALLBACK_EMBED=$(grep -c "Daemon embed failed; using local embedder" "${SEARCH_STDERR}" || true)
FALLBACK_RERANK=$(grep -c "Daemon rerank failed; using local reranker" "${SEARCH_STDERR}" || true)

count_fallback_reason() {
    local reason=$1
    grep -o "fallback_reason=${reason}" "${SEARCH_STDERR}" | wc -l | tr -d ' '
}

FALLBACK_UNAVAILABLE=$(count_fallback_reason "unavailable")
FALLBACK_TIMEOUT=$(count_fallback_reason "timeout")
FALLBACK_OVERLOADED=$(count_fallback_reason "overloaded")
FALLBACK_ERROR=$(count_fallback_reason "error")
FALLBACK_INVALID=$(count_fallback_reason "invalid")
FALLBACK_BACKOFF=$(count_fallback_reason "backoff")

BACKOFF_VALUES=$(grep -o "backoff_ms=[0-9]*" "${SEARCH_STDERR}" | awk -F= '{print $2}' || true)
if [[ -n "${BACKOFF_VALUES}" ]]; then
    BACKOFF_COUNT=$(echo "${BACKOFF_VALUES}" | wc -l | tr -d ' ')
    BACKOFF_MIN=$(echo "${BACKOFF_VALUES}" | sort -n | head -n 1)
    BACKOFF_MAX=$(echo "${BACKOFF_VALUES}" | sort -n | tail -n 1)
    BACKOFF_AVG=$(echo "${BACKOFF_VALUES}" | awk '{sum+=$1} END { if (NR>0) printf "%.2f", sum/NR; else print "0" }')
else
    BACKOFF_COUNT=0
    BACKOFF_MIN=0
    BACKOFF_MAX=0
    BACKOFF_AVG=0
fi

log "INFO" "Daemon embed attempts: ${ATTEMPT_EMBED}"
log "INFO" "Daemon rerank attempts: ${ATTEMPT_RERANK}"
log "INFO" "Embed fallbacks: ${FALLBACK_EMBED}"
log "INFO" "Rerank fallbacks: ${FALLBACK_RERANK}"
log "INFO" "Fallback reasons - unavailable=${FALLBACK_UNAVAILABLE} timeout=${FALLBACK_TIMEOUT} overloaded=${FALLBACK_OVERLOADED} error=${FALLBACK_ERROR} invalid=${FALLBACK_INVALID} backoff=${FALLBACK_BACKOFF}"
log "INFO" "Backoff samples: ${BACKOFF_COUNT} (min=${BACKOFF_MIN}ms max=${BACKOFF_MAX}ms avg=${BACKOFF_AVG}ms)"
log "INFO" "Search latency: ${SEARCH_LATENCY_MS}ms"

# Emit metrics event for daemon fallback
emit_metrics "daemon_fallback" \
    "latency_ms=${SEARCH_LATENCY_MS}" \
    "embed_attempts=${ATTEMPT_EMBED}" \
    "rerank_attempts=${ATTEMPT_RERANK}" \
    "embed_fallbacks=${FALLBACK_EMBED}" \
    "rerank_fallbacks=${FALLBACK_RERANK}" \
    "fallback_unavailable=${FALLBACK_UNAVAILABLE}" \
    "fallback_timeout=${FALLBACK_TIMEOUT}" \
    "fallback_overloaded=${FALLBACK_OVERLOADED}" \
    "fallback_error=${FALLBACK_ERROR}" \
    "backoff_count=${BACKOFF_COUNT}"

emit_phase_end "daemon_fallback_test"
e2e_phase_end "daemon_fallback_test" "$SEARCH_LATENCY_MS"

cat > "${REPORT_JSON}" <<EOF
{
  "run_id": "$(json_escape "$RUN_ID")",
  "timestamp": "$(date -u +"%Y-%m-%dT%H:%M:%SZ")",
  "runner": "bash",
  "test_summary": {
    "total": ${TESTS_TOTAL},
    "passed": ${TESTS_PASSED},
    "failed": ${TESTS_FAILED}
  },
  "query": "$(json_escape "$QUERY")",
  "embedder": "$(json_escape "$EMBEDDER")",
  "daemon_enabled": true,
  "retry_config": {
    "max_attempts": ${DAEMON_RETRY_MAX},
    "base_delay_ms": ${DAEMON_BACKOFF_BASE_MS},
    "max_delay_ms": ${DAEMON_BACKOFF_MAX_MS},
    "jitter_pct": ${DAEMON_JITTER_PCT}
  },
  "search_exit_code": ${SEARCH_EXIT},
  "latency_ms": ${SEARCH_LATENCY_MS},
  "attempts": {
    "embed": ${ATTEMPT_EMBED},
    "rerank": ${ATTEMPT_RERANK}
  },
  "fallbacks": {
    "embed": ${FALLBACK_EMBED},
    "rerank": ${FALLBACK_RERANK}
  },
  "fallback_reasons": {
    "unavailable": ${FALLBACK_UNAVAILABLE},
    "timeout": ${FALLBACK_TIMEOUT},
    "overloaded": ${FALLBACK_OVERLOADED},
    "error": ${FALLBACK_ERROR},
    "invalid": ${FALLBACK_INVALID},
    "backoff": ${FALLBACK_BACKOFF}
  },
  "backoff_ms": {
    "samples": ${BACKOFF_COUNT},
    "min": ${BACKOFF_MIN},
    "max": ${BACKOFF_MAX},
    "avg": ${BACKOFF_AVG}
  },
  "artifacts": {
    "jsonl": "$(json_escape "$JSONL_FILE")",
    "stdout": "$(json_escape "$SEARCH_STDOUT")",
    "stderr": "$(json_escape "$SEARCH_STDERR")",
    "log": "$(json_escape "$LOG_FILE")"
  }
}
EOF

popd >/dev/null

# Determine final exit code
FINAL_EXIT=0
if [[ $TESTS_FAILED -gt 0 ]]; then
    FINAL_EXIT=1
fi

# Emit run_end event
emit_run_end "$FINAL_EXIT"
E2E_TOTAL_DURATION=$(e2e_duration_since_start)
e2e_run_end "$TESTS_TOTAL" "$TESTS_PASSED" "$TESTS_FAILED" 0 "$E2E_TOTAL_DURATION"

if [[ $FINAL_EXIT -ne 0 ]]; then
    log "FAIL" "Daemon E2E run failed (${TESTS_FAILED}/${TESTS_TOTAL} tests failed)"
    log "INFO" "JSONL log: ${JSONL_FILE}"
    log "INFO" "Report: ${REPORT_JSON}"
    exit "$FINAL_EXIT"
fi

log "OK" "Daemon E2E run completed (${TESTS_PASSED}/${TESTS_TOTAL} tests passed)"
log "INFO" "JSONL log: ${JSONL_FILE}"
log "INFO" "Report: ${REPORT_JSON}"
exit 0
