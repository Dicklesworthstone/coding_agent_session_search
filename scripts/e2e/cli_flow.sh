#!/usr/bin/env bash
# scripts/e2e/cli_flow.sh
# End-to-end CLI flow harness with structured logging.
#
# Usage:
#   ./scripts/e2e/cli_flow.sh
#   CASS_BIN=target/debug/cass ./scripts/e2e/cli_flow.sh
#   ./scripts/e2e/cli_flow.sh --no-build --fail-fast
#
# Environment:
#   RCH_BIN         rch executable (default: rch)
#   RCH_TARGET_DIR  cargo target dir for offloaded cass build
#
# Artifacts:
#   target/e2e-cli/run_<timestamp>/
#     run.log, run.jsonl, summary.json
#     stdout/*.out, stderr/*.err
#     pages_export/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_cli_flow_e2e}"

# Source standard E2E logging library (emits to test-results/e2e/)
# shellcheck disable=SC1091
source "${PROJECT_ROOT}/scripts/lib/e2e_log.sh"
e2e_init "shell" "cli_flow"

RUN_ID="$(date +"%Y%m%d_%H%M%S")_${RANDOM}"
LOG_ROOT="${PROJECT_ROOT}/target/e2e-cli"
RUN_DIR="${LOG_ROOT}/run_${RUN_ID}"
LOG_FILE="${RUN_DIR}/run.log"
JSON_LOG_FILE="${RUN_DIR}/run.jsonl"
SUMMARY_JSON="${RUN_DIR}/summary.json"
STDOUT_DIR="${RUN_DIR}/stdout"
STDERR_DIR="${RUN_DIR}/stderr"

SANDBOX_DIR="${RUN_DIR}/sandbox"
DATA_DIR="${SANDBOX_DIR}/cass_data"
DB_PATH="${DATA_DIR}/agent_search.db"
CODEX_HOME="${SANDBOX_DIR}/.codex"
CLAUDE_HOME="${SANDBOX_DIR}/.claude"
PAGES_EXPORT_DIR="${RUN_DIR}/pages_export"

NO_BUILD=0
FAIL_FAST=0
KEEP_SANDBOX=0

for arg in "$@"; do
    case "$arg" in
        --no-build)
            NO_BUILD=1
            ;;
        --fail-fast)
            FAIL_FAST=1
            ;;
        --keep-sandbox)
            KEEP_SANDBOX=1
            ;;
        --help|-h)
            echo "Usage: $0 [--no-build] [--fail-fast] [--keep-sandbox]"
            exit 0
            ;;
    esac
done

mkdir -p "${RUN_DIR}" "${STDOUT_DIR}" "${STDERR_DIR}"

e2e_run_start

# Colors (only when stdout is a terminal)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED='' GREEN='' YELLOW='' BLUE='' CYAN='' BOLD='' NC=''
fi

log() {
    local level=$1
    shift
    local msg="$*"
    local ts
    ts=$(date +"%Y-%m-%d %H:%M:%S.%3N" 2>/dev/null || date +"%Y-%m-%d %H:%M:%S")

    local color="$NC"
    case "$level" in
        INFO) color="$GREEN" ;;
        WARN) color="$YELLOW" ;;
        ERROR) color="$RED" ;;
        DEBUG) color="$CYAN" ;;
        PHASE) color="$BOLD$BLUE" ;;
    esac

    echo -e "${color}[${ts}] [${level}]${NC} ${msg}"
    echo "[${ts}] [${level}] ${msg}" >> "${LOG_FILE}"
}

json_escape() {
    local s="$1"
    s=${s//\\/\\\\}
    s=${s//"/\\"}
    s=${s//$'\n'/\\n}
    s=${s//$'\r'/\\r}
    s=${s//$'\t'/\\t}
    printf '%s' "$s"
}

now_ms() {
    if command -v python3 >/dev/null 2>&1; then
        python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
    elif command -v python >/dev/null 2>&1; then
        python - <<'PY'
import time
print(int(time.time() * 1000))
PY
    else
        date +%s000
    fi
}

now_iso() {
    if command -v python3 >/dev/null 2>&1; then
        python3 - <<'PY'
import datetime
print(datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z")
PY
    elif command -v python >/dev/null 2>&1; then
        python - <<'PY'
import datetime
print(datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z")
PY
    else
        date -u +"%Y-%m-%dT%H:%M:%SZ"
    fi
}

STEP_JSONS=()
FAILED_STEPS=()
START_MS=$(now_ms)
START_ISO=$(now_iso)

run_step() {
    local name=$1
    shift
    local stdout_file="${STDOUT_DIR}/${name}.out"
    local stderr_file="${STDERR_DIR}/${name}.err"
    local start_ms
    local end_ms
    local duration_ms
    local exit_code

    start_ms=$(now_ms)
    e2e_test_start "$name" "cli_flow"

    log PHASE "STEP: ${name}"
    log INFO "Command: $*"

    set +e
    "$@" >"${stdout_file}" 2>"${stderr_file}"
    exit_code=$?
    set -e

    end_ms=$(now_ms)
    duration_ms=$((end_ms - start_ms))

    if [[ $exit_code -eq 0 ]]; then
        log INFO "${name}: OK (${duration_ms}ms)"
        e2e_test_pass "$name" "cli_flow" "$duration_ms"
    else
        log ERROR "${name}: FAIL (${exit_code}) in ${duration_ms}ms"
        e2e_test_fail "$name" "cli_flow" "$duration_ms" 0 "exit code ${exit_code}" "CommandFailed"
        FAILED_STEPS+=("${name}")
        if [[ $FAIL_FAST -eq 1 ]]; then
            log ERROR "Fail-fast enabled; aborting after ${name}."
        fi
    fi

    local cmd_str
    cmd_str=$(printf '%q ' "$@")
    cmd_str=${cmd_str% }

    local json_cmd
    json_cmd=$(json_escape "$cmd_str")

    local json_line
    json_line=$(printf '{"ts":"%s","event":"step_end","step":"%s","command":"%s","exit_code":%d,"duration_ms":%d,"stdout":"%s","stderr":"%s"}' \
        "$(now_iso)" \
        "$(json_escape "$name")" \
        "$json_cmd" \
        "$exit_code" \
        "$duration_ms" \
        "$(json_escape "$stdout_file")" \
        "$(json_escape "$stderr_file")")
    echo "$json_line" >> "${JSON_LOG_FILE}"

    STEP_JSONS+=("$json_line")

    if [[ $FAIL_FAST -eq 1 && $exit_code -ne 0 ]]; then
        write_summary
        exit $exit_code
    fi
}

emit_snapshot() {
    local env_json
    local config_json
    local system_json
    local sources_path
    local tui_path
    local watch_path
    local bookmarks_path

    sources_path="${SANDBOX_DIR}/.config/cass/sources.toml"
    tui_path="${DATA_DIR}/tui_state.json"
    watch_path="${DATA_DIR}/watch_state.json"
    bookmarks_path="${DATA_DIR}/bookmarks.db"

    env_json=$(cat <<'EOSNAPSHOT'
{
  "HOME": "__HOME__",
  "CASS_DATA_DIR": "__CASS_DATA_DIR__",
  "CASS_DB_PATH": "__CASS_DB_PATH__",
  "CODEX_HOME": "__CODEX_HOME__",
  "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT": "1",
  "NO_COLOR": "1",
  "CASS_NO_COLOR": "1"
}
EOSNAPSHOT
)

    env_json=${env_json/__HOME__/$(json_escape "$SANDBOX_DIR")}
    env_json=${env_json/__CASS_DATA_DIR__/$(json_escape "$DATA_DIR")}
    env_json=${env_json/__CASS_DB_PATH__/$(json_escape "$DB_PATH")}
    env_json=${env_json/__CODEX_HOME__/$(json_escape "$CODEX_HOME")}

    config_json=$(cat <<'EOSNAPSHOT'
{
  "sources_toml": {"path": "__SOURCES__", "exists": __SOURCES_EXISTS__},
  "tui_state": {"path": "__TUI__", "exists": __TUI_EXISTS__},
  "watch_state": {"path": "__WATCH__", "exists": __WATCH_EXISTS__},
  "bookmarks_db": {"path": "__BOOKMARKS__", "exists": __BOOKMARKS_EXISTS__}
}
EOSNAPSHOT
)

    config_json=${config_json/__SOURCES__/$(json_escape "$sources_path")}
    config_json=${config_json/__SOURCES_EXISTS__/$( [[ -f "$sources_path" ]] && echo true || echo false )}
    config_json=${config_json/__TUI__/$(json_escape "$tui_path")}
    config_json=${config_json/__TUI_EXISTS__/$( [[ -f "$tui_path" ]] && echo true || echo false )}
    config_json=${config_json/__WATCH__/$(json_escape "$watch_path")}
    config_json=${config_json/__WATCH_EXISTS__/$( [[ -f "$watch_path" ]] && echo true || echo false )}
    config_json=${config_json/__BOOKMARKS__/$(json_escape "$bookmarks_path")}
    config_json=${config_json/__BOOKMARKS_EXISTS__/$( [[ -f "$bookmarks_path" ]] && echo true || echo false )}

    system_json=$(cat <<'EOSNAPSHOT'
{
  "uname": "__UNAME__",
  "pwd": "__PWD__",
  "shell": "__SHELL__"
}
EOSNAPSHOT
)

    system_json=${system_json/__UNAME__/$(json_escape "$(uname -a 2>/dev/null || echo unknown)")}
    system_json=${system_json/__PWD__/$(json_escape "$PROJECT_ROOT")}
    system_json=${system_json/__SHELL__/$(json_escape "${SHELL:-unknown}")}

    local snapshot_line
    snapshot_line=$(printf '{"ts":"%s","event":"snapshot","env":%s,"config":%s,"system":%s}' \
        "$(now_iso)" \
        "$env_json" \
        "$config_json" \
        "$system_json")
    echo "$snapshot_line" >> "${JSON_LOG_FILE}"

    log INFO "Environment snapshot recorded"
}

write_summary() {
    local end_ms
    local end_iso
    local duration_ms
    local status
    local steps_json
    end_ms=$(now_ms)
    end_iso=$(now_iso)
    duration_ms=$((end_ms - START_MS))

    # Emit standard E2E run_end event
    local _total=${#STEP_JSONS[@]}
    local _failed=${#FAILED_STEPS[@]}
    local _passed=$((_total - _failed))
    e2e_run_end "$_total" "$_passed" "$_failed" 0 "$duration_ms"

    if [[ ${#FAILED_STEPS[@]} -eq 0 ]]; then
        status="ok"
    else
        status="failed"
    fi

    if [[ ${#STEP_JSONS[@]} -gt 0 ]]; then
        steps_json="["
        local first=1
        for entry in "${STEP_JSONS[@]}"; do
            if [[ $first -eq 1 ]]; then
                steps_json+="$entry"
                first=0
            else
                steps_json+=",$entry"
            fi
        done
        steps_json+="]"
    else
        steps_json="[]"
    fi

    local failed_json
    if [[ ${#FAILED_STEPS[@]} -gt 0 ]]; then
        failed_json=$(printf '"%s"' "${FAILED_STEPS[@]}" | sed 's/" "/","/g')
    else
        failed_json=""
    fi

    cat <<EOF > "${SUMMARY_JSON}"
{
  "run_id": "${RUN_ID}",
  "status": "${status}",
  "started_at": "${START_ISO}",
  "ended_at": "${end_iso}",
  "duration_ms": ${duration_ms},
  "paths": {
    "run_dir": "$(json_escape "$RUN_DIR")",
    "run_log": "$(json_escape "$LOG_FILE")",
    "run_jsonl": "$(json_escape "$JSON_LOG_FILE")",
    "stdout_dir": "$(json_escape "$STDOUT_DIR")",
    "stderr_dir": "$(json_escape "$STDERR_DIR")",
    "pages_export": "$(json_escape "$PAGES_EXPORT_DIR")",
    "cass_bin": "$(json_escape "${CASS_BIN_RESOLVED:-unknown}")"
  },
  "sandbox": {
    "home": "$(json_escape "$SANDBOX_DIR")",
    "data_dir": "$(json_escape "$DATA_DIR")",
    "db_path": "$(json_escape "$DB_PATH")"
  },
  "steps": ${steps_json},
  "failed_steps": [${failed_json}]
}
EOF
}

log PHASE "cass E2E CLI Flow Harness"
log INFO "Run directory: ${RUN_DIR}"
log INFO "Log file: ${LOG_FILE}"
log INFO "JSON log: ${JSON_LOG_FILE}"

# Setup sandbox and fixtures
mkdir -p "${CODEX_HOME}/sessions/2024/12/01"
mkdir -p "${CLAUDE_HOME}/projects/myapp"
mkdir -p "${DATA_DIR}"

cat <<'EOSAMPLE' > "${CODEX_HOME}/sessions/2024/12/01/rollout-test.jsonl"
{"type":"event_msg","timestamp":1733011200000,"payload":{"type":"user_message","message":"authentication error in login"}}
{"type":"response_item","timestamp":1733011201000,"payload":{"role":"assistant","content":"authentication error in login_response"}}
EOSAMPLE

cat <<'EOSAMPLE' > "${CLAUDE_HOME}/projects/myapp/session.jsonl"
{"type":"user","timestamp":"2024-12-01T10:00:00Z","message":{"role":"user","content":"fix the database connection"}}
{"type":"assistant","timestamp":"2024-12-01T10:01:00Z","message":{"role":"assistant","content":"fix the database connection_response"}}
EOSAMPLE

emit_snapshot

CASS_ENV=(
    "HOME=${SANDBOX_DIR}"
    "CODEX_HOME=${CODEX_HOME}"
    "CASS_DATA_DIR=${DATA_DIR}"
    "CASS_DB_PATH=${DB_PATH}"
    "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1"
    "NO_COLOR=1"
    "CASS_NO_COLOR=1"
)

# shellcheck disable=SC2317
ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        log ERROR "rch binary not found; CLI flow E2E cass build must be offloaded"
        write_summary
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
    (cd "$PROJECT_ROOT" && run_cargo build --bin cass)
}

if [[ -n "${CASS_BIN:-}" ]]; then
    CASS_BIN_RESOLVED="$CASS_BIN"
elif [[ $NO_BUILD -eq 1 ]]; then
    CASS_BIN_RESOLVED="${PROJECT_ROOT}/target/debug/cass"
else
    CASS_BIN_RESOLVED="${RCH_TARGET_DIR}/debug/cass"
fi

if [[ ! -x "$CASS_BIN_RESOLVED" ]]; then
    if [[ $NO_BUILD -eq 1 ]]; then
        log ERROR "CASS_BIN not found at ${CASS_BIN_RESOLVED} and --no-build set."
        write_summary
        exit 1
    fi
    run_step "build" build_cass_binary
fi

log INFO "Using cass binary: ${CASS_BIN_RESOLVED}"

# CLI flows
run_step "index" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" index --full --json --data-dir "${DATA_DIR}"

run_step "health" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" health --json --data-dir "${DATA_DIR}" --robot-meta

run_step "search" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" search "authentication" --robot --limit 5 --data-dir "${DATA_DIR}"

run_step "view" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" view --json "${CODEX_HOME}/sessions/2024/12/01/rollout-test.jsonl"

run_step "expand" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" expand --json "${CODEX_HOME}/sessions/2024/12/01/rollout-test.jsonl" -n 1 -C 2

run_step "pages_export" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" pages --export-only "${PAGES_EXPORT_DIR}" --path-mode relative

run_step "sources_list" env "${CASS_ENV[@]}" "${CASS_BIN_RESOLVED}" \
    --db "${DB_PATH}" sources list --json

write_summary

log PHASE "Run complete"
log INFO "Summary: ${SUMMARY_JSON}"
log INFO "Failed steps: ${#FAILED_STEPS[@]}"

if [[ ${#FAILED_STEPS[@]} -ne 0 ]]; then
    log ERROR "One or more steps failed."
    exit 1
fi

if [[ $KEEP_SANDBOX -eq 0 ]]; then
    log INFO "Sandbox preserved in ${SANDBOX_DIR} (no cleanup to avoid destructive ops)."
else
    log INFO "Sandbox preserved in ${SANDBOX_DIR}."
fi

exit 0
