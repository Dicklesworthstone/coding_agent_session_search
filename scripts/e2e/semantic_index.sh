#!/usr/bin/env bash
# scripts/e2e/semantic_index.sh
# End-to-end semantic indexing workflow test with structured logs.
#
# Usage:
#   ./scripts/e2e/semantic_index.sh
#   ./scripts/e2e/semantic_index.sh --no-build
#   ./scripts/e2e/semantic_index.sh --embedder hash
#
# Artifacts:
#   target/e2e-semantic/run_<timestamp>/
#     run.log, run.jsonl, summary.json
#     stdout/*.out, stderr/*.err
#     sandbox/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

RUN_ID="$(date +"%Y%m%d_%H%M%S")_${RANDOM}"
LOG_ROOT="${PROJECT_ROOT}/target/e2e-semantic"
RUN_DIR="${LOG_ROOT}/run_${RUN_ID}"
LOG_FILE="${RUN_DIR}/run.log"
JSON_LOG_FILE="${RUN_DIR}/run.jsonl"
SUMMARY_JSON="${RUN_DIR}/summary.json"
STDOUT_DIR="${RUN_DIR}/stdout"
STDERR_DIR="${RUN_DIR}/stderr"

SANDBOX_DIR="${RUN_DIR}/sandbox"
BUILD_TARGET_DIR="${RUN_DIR}/target"
DATA_DIR="${SANDBOX_DIR}/cass_data"
CODEX_HOME="${SANDBOX_DIR}/.codex"
HOME_DIR="${SANDBOX_DIR}/home"

NO_BUILD=0
REQUESTED_EMBEDDER="fastembed"
SKIP_MODEL_INSTALL=0
FAIL_FAST=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)
            NO_BUILD=1
            shift
            ;;
        --embedder)
            shift
            if [[ $# -gt 0 ]]; then
                REQUESTED_EMBEDDER="$1"
                shift
            else
                REQUESTED_EMBEDDER="fastembed"
            fi
            ;;
        --skip-model-install)
            SKIP_MODEL_INSTALL=1
            shift
            ;;
        --fail-fast)
            FAIL_FAST=1
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [--no-build] [--embedder fastembed|hash] [--skip-model-install] [--fail-fast]"
            exit 0
            ;;
        *)
            shift
            ;;
    esac
done

mkdir -p "${RUN_DIR}" "${STDOUT_DIR}" "${STDERR_DIR}" "${SANDBOX_DIR}" "${DATA_DIR}" "${CODEX_HOME}" "${HOME_DIR}"

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

now_iso() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

STEP_JSONS=()
FAILED_STEPS=()

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
        FAILED_STEPS+=("${name}")
    fi

    local cmd_str
    cmd_str=$(printf '%q ' "$@")
    cmd_str=${cmd_str% }

    local json_line
    json_line=$(printf '{"ts":"%s","event":"step","step":"%s","command":"%s","exit_code":%d,"stdout":"%s","stderr":"%s"}' \
        "$(now_iso)" \
        "$(json_escape "$name")" \
        "$(json_escape "$cmd_str")" \
        "$exit_code" \
        "$(json_escape "$stdout_file")" \
        "$(json_escape "$stderr_file")")
    echo "$json_line" >> "${JSON_LOG_FILE}"
    STEP_JSONS+=("$json_line")

    if [[ $FAIL_FAST -eq 1 && $exit_code -ne 0 ]]; then
        write_summary
        exit "$exit_code"
    fi

    return 0
}

run_step_optional() {
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
        log "WARN" "${name} (exit ${exit_code})"
    fi

    local cmd_str
    cmd_str=$(printf '%q ' "$@")
    cmd_str=${cmd_str% }

    local json_line
    json_line=$(printf '{"ts":"%s","event":"step_optional","step":"%s","command":"%s","exit_code":%d,"stdout":"%s","stderr":"%s"}' \
        "$(now_iso)" \
        "$(json_escape "$name")" \
        "$(json_escape "$cmd_str")" \
        "$exit_code" \
        "$(json_escape "$stdout_file")" \
        "$(json_escape "$stderr_file")")
    echo "$json_line" >> "${JSON_LOG_FILE}"
    STEP_JSONS+=("$json_line")

    return 0
}

json_state() {
    local file=$1
    if command -v jq >/dev/null 2>&1; then
        jq -r '.state // "unknown"' "$file" 2>/dev/null || echo "unknown"
        return 0
    fi
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$file" <<'PY' || echo "unknown"
import json, sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    data = json.load(f)
print(data.get("state", "unknown"))
PY
        return 0
    fi
    echo "unknown"
}

json_hits_count() {
    local file=$1
    if command -v jq >/dev/null 2>&1; then
        jq -r '.hits | length' "$file" 2>/dev/null || echo "0"
        return 0
    fi
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$file" <<'PY' || echo "0"
import json, sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    data = json.load(f)
print(len(data.get("hits", [])))
PY
        return 0
    fi
    echo "0"
}

write_summary() {
    local status="ok"
    if [[ ${#FAILED_STEPS[@]} -gt 0 ]]; then
        status="fail"
    fi

    {
        echo "{"
        echo "  \"run_id\": \"${RUN_ID}\","
        echo "  \"status\": \"${status}\","
        echo "  \"failed_steps\": ["
        local first=1
        for step in "${FAILED_STEPS[@]}"; do
            if [[ $first -eq 0 ]]; then
                echo ","
            fi
            first=0
            printf '    "%s"' "$(json_escape "$step")"
        done
        echo ""
        echo "  ]"
        echo "}"
    } > "${SUMMARY_JSON}"
}

log "INFO" "Run directory: ${RUN_DIR}"

if [[ $NO_BUILD -eq 0 ]]; then
    run_step "build" bash -c "cd \"$PROJECT_ROOT\" && CARGO_TARGET_DIR=\"$BUILD_TARGET_DIR\" cargo build"
fi

if [[ -z "${CASS_BIN:-}" ]]; then
    if [[ $NO_BUILD -eq 0 ]]; then
        CASS_BIN="${BUILD_TARGET_DIR}/debug/cass"
    else
        CASS_BIN="${PROJECT_ROOT}/target/debug/cass"
    fi
fi

if [[ ! -x "$CASS_BIN" ]]; then
    log "FAIL" "cass binary not found or not executable at ${CASS_BIN}"
    write_summary
    exit 1
fi

run_step "version" "$CASS_BIN" --version

log "INFO" "Preparing sandbox data"
mkdir -p "${CODEX_HOME}/sessions/2024/11/20"
cat > "${CODEX_HOME}/sessions/2024/11/20/rollout-1.jsonl" <<'JSONL'
{"type":"event_msg","timestamp":1732118400000,"payload":{"type":"user_message","message":"Implement a binary search algorithm"}}
{"type":"response_item","timestamp":1732118401000,"payload":{"role":"assistant","content":"Binary search halves the search space each step."}}
{"type":"event_msg","timestamp":1732118402000,"payload":{"type":"user_message","message":"Handle edge cases like empty arrays."}}
{"type":"response_item","timestamp":1732118403000,"payload":{"role":"assistant","content":"Check for empty arrays before indexing."}}
JSONL

export CASS_DATA_DIR="${DATA_DIR}"
export CODEX_HOME="${CODEX_HOME}"
export HOME="${HOME_DIR}"
export CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1

pushd "${SANDBOX_DIR}" >/dev/null

run_step "models_status" "$CASS_BIN" models status --json

MODEL_STATE=$(json_state "${STDOUT_DIR}/models_status.out")

if [[ "$MODEL_STATE" != "ready" && $SKIP_MODEL_INSTALL -eq 0 ]]; then
    run_step_optional "models_install" "$CASS_BIN" models install --data-dir "${DATA_DIR}" -y
    run_step_optional "models_status_after_install" "$CASS_BIN" models status --json
    if [[ -f "${STDOUT_DIR}/models_status_after_install.out" ]]; then
        MODEL_STATE=$(json_state "${STDOUT_DIR}/models_status_after_install.out")
    fi
fi

run_step "index_full" "$CASS_BIN" index --full --data-dir "${DATA_DIR}"
ACTIVE_EMBEDDER="${REQUESTED_EMBEDDER}"
if [[ "${REQUESTED_EMBEDDER}" == "fastembed" && "${MODEL_STATE}" != "ready" ]]; then
    log "WARN" "Model not ready; falling back to hash embedder"
    ACTIVE_EMBEDDER="hash"
fi

run_step "index_semantic" "$CASS_BIN" index --semantic --embedder "${ACTIVE_EMBEDDER}" --data-dir "${DATA_DIR}"

SEARCH_MODEL_FLAGS=()
if [[ "${ACTIVE_EMBEDDER}" == "hash" ]]; then
    SEARCH_MODEL_FLAGS=(--model hash)
fi

INDEX_DIR="${DATA_DIR}/vector_index"
if ls "${INDEX_DIR}"/index-*.cvvi >/dev/null 2>&1; then
    log "OK" "Vector index file present in ${INDEX_DIR}"
else
    log "FAIL" "Vector index file missing in ${INDEX_DIR}"
    FAILED_STEPS+=("index_file_missing")
fi

run_step "search_semantic" "$CASS_BIN" search "binary search" --mode semantic --json --data-dir "${DATA_DIR}" "${SEARCH_MODEL_FLAGS[@]}"
run_step "search_lexical" "$CASS_BIN" search "binary search" --mode lexical --json --data-dir "${DATA_DIR}"

SEM_COUNT=$(json_hits_count "${STDOUT_DIR}/search_semantic.out")
LEX_COUNT=$(json_hits_count "${STDOUT_DIR}/search_lexical.out")

log "INFO" "Semantic hits: ${SEM_COUNT}, Lexical hits: ${LEX_COUNT}"

if [[ "${SEM_COUNT}" -le 0 ]]; then
    log "FAIL" "Semantic search returned no hits"
    FAILED_STEPS+=("semantic_hits_empty")
fi

popd >/dev/null

write_summary

if [[ ${#FAILED_STEPS[@]} -gt 0 ]]; then
    log "FAIL" "Some steps failed. See ${SUMMARY_JSON}"
    exit 1
fi

log "OK" "Semantic indexing E2E flow passed"
exit 0
