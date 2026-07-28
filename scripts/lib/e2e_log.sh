#!/usr/bin/env bash
# scripts/lib/e2e_log.sh
# Unified E2E logging library for shell scripts.
#
# Implements the schema from docs/reference/E2E_LOGGING_SCHEMA.md
#
# Usage:
#   source scripts/lib/e2e_log.sh
#   e2e_init "shell" "my_script"
#   e2e_run_start
#   e2e_test_start "test_name" "suite_name"
#   e2e_test_pass "test_name" "suite_name" 1000
#   e2e_run_end 1 1 0 0 1000

# =============================================================================
# Configuration
# =============================================================================

# Global state (initialized by e2e_init)
E2E_RUNNER=""
E2E_SCRIPT_NAME=""
E2E_RUN_ID=""
E2E_OUTPUT_DIR=""
E2E_OUTPUT_FILE=""
E2E_START_TIME=""
E2E_VERBOSE_ENABLED="false"
E2E_VERBOSE_LOG=""

# =============================================================================
# Internal Helpers
# =============================================================================

_e2e_timestamp() {
    # ISO-8601 timestamp with milliseconds
    if date --version 2>/dev/null | grep -q GNU; then
        date -u +"%Y-%m-%dT%H:%M:%S.%3NZ"
    else
        # macOS/BSD fallback (no milliseconds support)
        date -u +"%Y-%m-%dT%H:%M:%S.000Z"
    fi
}

_e2e_timestamp_id() {
    date -u +"%Y%m%d_%H%M%S"
}

_e2e_random_suffix() {
    # Generate a short random hex suffix
    printf '%06x' $((RANDOM * RANDOM % 16777216))
}

_e2e_validate_run_id() {
    local run_id="$1"
    if [[ ${#run_id} -lt 8 || ${#run_id} -gt 128 ]] \
        || [[ ! "$run_id" =~ ^[A-Za-z0-9][A-Za-z0-9_-]*[A-Za-z0-9]$ ]]; then
        echo "Error: CASS_E2E_RUN_ID must be 8-128 ASCII alphanumeric, '_' or '-' bytes" >&2
        return 2
    fi
}

_e2e_json_escape() {
    # Escape a string for JSON
    local s="$1"
    s="${s//\\/\\\\}"      # backslash
    s="${s//\"/\\\"}"      # double quote
    s="${s//$'\n'/\\n}"    # newline
    s="${s//$'\r'/\\r}"    # carriage return
    s="${s//$'\t'/\\t}"    # tab
    echo "$s"
}

_e2e_git_sha() {
    git rev-parse --short HEAD 2>/dev/null || echo "null"
}

_e2e_git_branch() {
    git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "null"
}

_e2e_os() {
    uname -s | tr '[:upper:]' '[:lower:]'
}

_e2e_arch() {
    local arch
    arch=$(uname -m)
    case "$arch" in
        x86_64) echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) echo "$arch" ;;
    esac
}

_e2e_rust_version() {
    rustc --version 2>/dev/null | awk '{print $2}' || echo "null"
}

_e2e_node_version() {
    node --version 2>/dev/null || echo "null"
}

_e2e_cass_version() {
    if [[ -n "${CARGO_PKG_VERSION:-}" ]]; then
        echo "$CARGO_PKG_VERSION"
    elif command -v cass &>/dev/null; then
        cass --version 2>/dev/null | awk '{print $2}' || echo "null"
    else
        echo "null"
    fi
}

_e2e_is_ci() {
    if [[ -n "${CI:-}" ]] || [[ -n "${GITHUB_ACTIONS:-}" ]]; then
        echo "true"
    else
        echo "false"
    fi
}

_e2e_write() {
    # Write a line to the JSONL output file
    local line="$1"
    echo "$line" >> "$E2E_OUTPUT_FILE"
}

_e2e_verbose_write() {
    if [[ "$E2E_VERBOSE_ENABLED" != "true" ]]; then
        return
    fi
    local line="$1"
    local ts
    ts=$(_e2e_timestamp)
    echo "[$ts] $line" >> "$E2E_VERBOSE_LOG"
}

# Public helper for scripts
verbose_log() {
    _e2e_verbose_write "$*"
}

# =============================================================================
# Public API
# =============================================================================

# Initialize the E2E logger
# Usage: e2e_init "shell" "script_name"
e2e_init() {
    local runner="${1:-shell}"
    local script_name="${2:-unknown}"

    E2E_RUNNER="$runner"
    E2E_SCRIPT_NAME="$script_name"

    local timestamp_id random_suffix
    timestamp_id=$(_e2e_timestamp_id)
    random_suffix=$(_e2e_random_suffix)
    if [[ -n "${CASS_E2E_RUN_ID:-}" ]]; then
        _e2e_validate_run_id "$CASS_E2E_RUN_ID" || return
        E2E_RUN_ID="$CASS_E2E_RUN_ID"
    else
        E2E_RUN_ID="${timestamp_id}_${random_suffix}"
    fi

    # Determine output directory (relative to project root)
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    local project_root
    project_root="$(cd "$script_dir/../.." && pwd)"

    E2E_OUTPUT_DIR="${project_root}/test-results/e2e"
    if [[ -n "${CASS_E2E_RUN_ID:-}" ]]; then
        E2E_OUTPUT_DIR="${E2E_OUTPUT_DIR}/runs/${E2E_RUN_ID}"
    fi
    mkdir -p "$E2E_OUTPUT_DIR"

    E2E_OUTPUT_FILE="${E2E_OUTPUT_DIR}/${runner}_${script_name}_${timestamp_id}_${random_suffix}.jsonl"
    E2E_START_TIME=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

    if [[ -n "${E2E_VERBOSE:-}" ]]; then
        E2E_VERBOSE_ENABLED="true"
        E2E_VERBOSE_LOG="${E2E_OUTPUT_DIR}/verbose_${runner}_${script_name}_${timestamp_id}.log"
        _e2e_verbose_write "Verbose logging enabled for ${runner}:${script_name}"
    fi
}

# Emit a run_start event
# Usage: e2e_run_start [test_filter] [parallel] [fail_fast]
e2e_run_start() {
    local test_filter="${1:-}"
    local parallel="${2:-false}"
    local fail_fast="${3:-false}"

    local ts
    ts=$(_e2e_timestamp)

    local git_sha git_branch os arch rust_version node_version cass_version ci
    git_sha=$(_e2e_git_sha)
    git_branch=$(_e2e_git_branch)
    os=$(_e2e_os)
    arch=$(_e2e_arch)
    rust_version=$(_e2e_rust_version)
    node_version=$(_e2e_node_version)
    cass_version=$(_e2e_cass_version)
    ci=$(_e2e_is_ci)

    # Build JSON manually (portable, no jq dependency)
    local json
    json=$(cat <<EOF
{"ts":"$ts","event":"run_start","run_id":"$E2E_RUN_ID","runner":"$E2E_RUNNER","env":{"git_sha":"$git_sha","git_branch":"$git_branch","os":"$os","arch":"$arch","rust_version":"$rust_version","node_version":"$node_version","cass_version":"$cass_version","ci":$ci},"config":{"test_filter":"$test_filter","parallel":$parallel,"fail_fast":$fail_fast}}
EOF
)
    _e2e_write "$json"
    _e2e_verbose_write "RUN_START run_id=${E2E_RUN_ID} runner=${E2E_RUNNER} parallel=${parallel} fail_fast=${fail_fast}"
}

# Emit a test_start event
# Usage: e2e_test_start "test_name" "suite_name" [file] [line]
e2e_test_start() {
    local test_name="$1"
    local suite_name="$2"
    local file="${3:-}"
    local line="${4:-}"

    local ts
    ts=$(_e2e_timestamp)

    local file_json=""
    [[ -n "$file" ]] && file_json=",\"file\":\"$file\""

    local line_json=""
    [[ -n "$line" ]] && line_json=",\"line\":$line"

    local json
    json="{\"ts\":\"$ts\",\"event\":\"test_start\",\"run_id\":\"$E2E_RUN_ID\",\"runner\":\"$E2E_RUNNER\",\"test\":{\"name\":\"$test_name\",\"suite\":\"$suite_name\"$file_json$line_json}}"
    _e2e_write "$json"
    _e2e_verbose_write "TEST_START name=${test_name} suite=${suite_name}"
}

# Emit a test_end event for a passing test
# Usage: e2e_test_pass "test_name" "suite_name" duration_ms [retries]
e2e_test_pass() {
    local test_name="$1"
    local suite_name="$2"
    local duration_ms="$3"
    local retries="${4:-0}"

    _e2e_test_end "$test_name" "$suite_name" "pass" "$duration_ms" "$retries" "" "" ""
}

# Emit a test_end event for a failing test
# Usage: e2e_test_fail "test_name" "suite_name" duration_ms [retries] [error_msg] [error_type] [stack]
e2e_test_fail() {
    local test_name="$1"
    local suite_name="$2"
    local duration_ms="$3"
    local retries="${4:-0}"
    local error_msg="${5:-test failed}"
    local error_type="${6:-}"
    local stack="${7:-}"

    _e2e_test_end "$test_name" "$suite_name" "fail" "$duration_ms" "$retries" "$error_msg" "$error_type" "$stack"
}

# Emit a test_end event for a skipped test
# Usage: e2e_test_skip "test_name" "suite_name"
e2e_test_skip() {
    local test_name="$1"
    local suite_name="$2"

    _e2e_test_end "$test_name" "$suite_name" "skip" "0" "0" "" "" ""
}

# Internal: emit a test_end event
_e2e_test_end() {
    local test_name="$1"
    local suite_name="$2"
    local test_status="$3"
    local duration_ms="$4"
    local retries="$5"
    local error_msg="$6"
    local error_type="$7"
    local stack="$8"

    local ts
    ts=$(_e2e_timestamp)

    local error_json=""
    if [[ -n "$error_msg" ]]; then
        local escaped_msg escaped_stack
        escaped_msg=$(_e2e_json_escape "$error_msg")
        escaped_stack=$(_e2e_json_escape "$stack")

        error_json=",\"error\":{\"message\":\"$escaped_msg\""
        [[ -n "$error_type" ]] && error_json="$error_json,\"type\":\"$error_type\""
        [[ -n "$stack" ]] && error_json="$error_json,\"stack\":\"$escaped_stack\""
        error_json="$error_json}"
    fi

    local json
    json="{\"ts\":\"$ts\",\"event\":\"test_end\",\"run_id\":\"$E2E_RUN_ID\",\"runner\":\"$E2E_RUNNER\",\"test\":{\"name\":\"$test_name\",\"suite\":\"$suite_name\"},\"result\":{\"status\":\"$test_status\",\"duration_ms\":$duration_ms,\"retries\":$retries}$error_json}"
    _e2e_write "$json"
    if [[ -n "$error_msg" ]]; then
        _e2e_verbose_write "TEST_END name=${test_name} suite=${suite_name} status=${test_status} duration_ms=${duration_ms} error=\"${error_msg}\""
    else
        _e2e_verbose_write "TEST_END name=${test_name} suite=${suite_name} status=${test_status} duration_ms=${duration_ms}"
    fi
}

# Emit a run_end event
# Usage: e2e_run_end total passed failed skipped duration_ms [flaky]
e2e_run_end() {
    local total="$1"
    local passed="$2"
    local failed="$3"
    local skipped="$4"
    local duration_ms="$5"
    local flaky="${6:-0}"

    local exit_code=0
    [[ "$failed" -gt 0 ]] && exit_code=1

    local ts
    ts=$(_e2e_timestamp)

    local json
    json="{\"ts\":\"$ts\",\"event\":\"run_end\",\"run_id\":\"$E2E_RUN_ID\",\"runner\":\"$E2E_RUNNER\",\"summary\":{\"total\":$total,\"passed\":$passed,\"failed\":$failed,\"skipped\":$skipped,\"flaky\":$flaky,\"duration_ms\":$duration_ms},\"exit_code\":$exit_code}"
    _e2e_write "$json"
    _e2e_verbose_write "RUN_END total=${total} passed=${passed} failed=${failed} skipped=${skipped} flaky=${flaky} duration_ms=${duration_ms} exit_code=${exit_code}"
}

# Emit a log event
# Usage: e2e_log "INFO" "message" [phase] [command]
e2e_log() {
    local level="$1"
    local msg="$2"
    local phase="${3:-}"
    local command="${4:-}"

    local ts
    ts=$(_e2e_timestamp)

    local escaped_msg
    escaped_msg=$(_e2e_json_escape "$msg")

    local context_json=""
    if [[ -n "$phase" ]] || [[ -n "$command" ]]; then
        context_json=",\"context\":{"
        local first=true
        if [[ -n "$phase" ]]; then
            context_json="${context_json}\"phase\":\"$phase\""
            first=false
        fi
        if [[ -n "$command" ]]; then
            local escaped_cmd
            escaped_cmd=$(_e2e_json_escape "$command")
            [[ "$first" == "false" ]] && context_json="${context_json},"
            context_json="${context_json}\"command\":\"$escaped_cmd\""
        fi
        context_json="${context_json}}"
    fi

    local json
    json="{\"ts\":\"$ts\",\"event\":\"log\",\"run_id\":\"$E2E_RUN_ID\",\"runner\":\"$E2E_RUNNER\",\"level\":\"$level\",\"msg\":\"$escaped_msg\"$context_json}"
    _e2e_write "$json"
    _e2e_verbose_write "LOG level=${level} msg=${msg}"
}

# Convenience: log at INFO level
e2e_info() {
    e2e_log "INFO" "$@"
}

# Convenience: log at WARN level
e2e_warn() {
    e2e_log "WARN" "$@"
}

# Convenience: log at ERROR level
e2e_error() {
    e2e_log "ERROR" "$@"
}

# Convenience: log at DEBUG level
e2e_debug() {
    e2e_log "DEBUG" "$@"
}

# Emit a phase_start event
# Usage: e2e_phase_start "phase_name" [description]
e2e_phase_start() {
    local name="$1"
    local description="${2:-}"

    local ts
    ts=$(_e2e_timestamp)

    local desc_json=""
    [[ -n "$description" ]] && desc_json=",\"description\":\"$description\""

    local json
    json="{\"ts\":\"$ts\",\"event\":\"phase_start\",\"run_id\":\"$E2E_RUN_ID\",\"runner\":\"$E2E_RUNNER\",\"phase\":{\"name\":\"$name\"$desc_json}}"
    _e2e_write "$json"
}

# Emit a phase_end event
# Usage: e2e_phase_end "phase_name" duration_ms
e2e_phase_end() {
    local name="$1"
    local duration_ms="$2"

    local ts
    ts=$(_e2e_timestamp)

    local json
    json="{\"ts\":\"$ts\",\"event\":\"phase_end\",\"run_id\":\"$E2E_RUN_ID\",\"runner\":\"$E2E_RUNNER\",\"phase\":{\"name\":\"$name\"},\"duration_ms\":$duration_ms}"
    _e2e_write "$json"
}

# Get the output file path
e2e_output_file() {
    echo "$E2E_OUTPUT_FILE"
}

# Get the run ID
e2e_run_id() {
    echo "$E2E_RUN_ID"
}

# Calculate duration from start time
# Usage: e2e_duration_since_start
e2e_duration_since_start() {
    local now
    now=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    echo $((now - E2E_START_TIME))
}

# =============================================================================
# Test Harness Helpers
# =============================================================================

# Run a test function and log results
# Usage: e2e_run_test "test_name" "suite" test_function [args...]
e2e_run_test() {
    local test_name="$1"
    local suite="$2"
    shift 2
    local test_fn="$1"
    shift

    e2e_test_start "$test_name" "$suite"

    local start_time
    start_time=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

    local exit_code=0
    local error_output=""

    # Run the test function, capturing stderr
    error_output=$("$test_fn" "$@" 2>&1) || exit_code=$?

    local end_time
    end_time=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    local duration=$((end_time - start_time))

    if [[ $exit_code -eq 0 ]]; then
        e2e_test_pass "$test_name" "$suite" "$duration"
        return 0
    else
        e2e_test_fail "$test_name" "$suite" "$duration" 0 "$error_output" "TestFailure"
        return $exit_code
    fi
}

# =============================================================================
# Failure State Dump (rtpd)
# =============================================================================

# Dump failure state for debugging
# Creates a comprehensive diagnostic dump when a test fails
#
# Usage: e2e_dump_failure_state "test_name" [temp_dir] [log_file] [db_path]
#
# Output: test-results/failure_dumps/<test_name>_<timestamp>.txt
#
e2e_dump_failure_state() {
    local test_name="$1"
    local temp_dir="${2:-}"
    local log_file="${3:-}"
    local db_path="${4:-}"

    # Create failure dumps directory
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    local project_root
    project_root="$(cd "$script_dir/../.." && pwd)"
    local dump_dir="${project_root}/test-results/failure_dumps"
    mkdir -p "$dump_dir"

    # Generate unique dump filename
    local timestamp
    timestamp=$(date +"%Y%m%d_%H%M%S")
    local dump_file="${dump_dir}/${test_name}_${timestamp}.txt"

    {
        echo "=========================================="
        echo "  FAILURE STATE DUMP"
        echo "=========================================="
        echo ""
        echo "Test: $test_name"
        echo "Time: $(date -Iseconds)"
        echo "Run ID: ${E2E_RUN_ID:-unknown}"
        echo ""

        # 1. Environment variables (sanitized)
        echo "=========================================="
        echo "  ENVIRONMENT"
        echo "=========================================="
        echo ""
        env | sort | while read -r line; do
            # Redact sensitive values
            case "$line" in
                *PASSWORD*|*SECRET*|*TOKEN*|*KEY*|*CREDENTIAL*)
                    echo "${line%%=*}=[REDACTED]"
                    ;;
                *)
                    echo "$line"
                    ;;
            esac
        done
        echo ""

        # 2. Working directory
        echo "=========================================="
        echo "  WORKING DIRECTORY"
        echo "=========================================="
        echo ""
        echo "CWD: $(pwd)"
        echo ""
        ls -la 2>/dev/null || echo "(unable to list directory)"
        echo ""

        # 3. Temp directory listing
        if [[ -n "$temp_dir" ]] && [[ -d "$temp_dir" ]]; then
            echo "=========================================="
            echo "  TEMP DIRECTORY: $temp_dir"
            echo "=========================================="
            echo ""
            ls -laR "$temp_dir" 2>/dev/null || echo "(unable to list temp dir)"
            echo ""
        fi

        # 4. Log tail
        if [[ -n "$log_file" ]] && [[ -f "$log_file" ]]; then
            echo "=========================================="
            echo "  LOG TAIL (last 100 lines): $log_file"
            echo "=========================================="
            echo ""
            tail -100 "$log_file" 2>/dev/null || echo "(unable to read log)"
            echo ""
        fi

        # 5. Database state
        if [[ -n "$db_path" ]] && [[ -f "$db_path" ]]; then
            echo "=========================================="
            echo "  DATABASE STATE: $db_path"
            echo "=========================================="
            echo ""
            if command -v sqlite3 &>/dev/null; then
                echo "--- Schema ---"
                sqlite3 "$db_path" ".schema" 2>/dev/null || echo "(unable to read schema)"
                echo ""
                echo "--- Table counts ---"
                sqlite3 "$db_path" "SELECT name || ': ' || (SELECT COUNT(*) FROM main." || name || ") FROM sqlite_master WHERE type='table';" 2>/dev/null || echo "(unable to count tables)"
            else
                echo "(sqlite3 not available)"
            fi
            echo ""
        fi

        # 6. Git state
        echo "=========================================="
        echo "  GIT STATE"
        echo "=========================================="
        echo ""
        echo "Branch: $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo 'unknown')"
        echo "Commit: $(git rev-parse --short HEAD 2>/dev/null || echo 'unknown')"
        echo ""
        echo "--- Uncommitted changes ---"
        git status --short 2>/dev/null || echo "(not a git repo or git unavailable)"
        echo ""

        # 7. Process info
        echo "=========================================="
        echo "  PROCESS INFO"
        echo "=========================================="
        echo ""
        echo "PID: $$"
        echo "User: $(whoami)"
        if command -v free &>/dev/null; then
            echo ""
            echo "--- Memory ---"
            free -h 2>/dev/null || true
        fi
        if [[ -f /proc/self/fd ]]; then
            echo ""
            echo "--- Open file handles ---"
            ls -la /proc/self/fd 2>/dev/null | head -20 || true
        fi
        echo ""

        echo "=========================================="
        echo "  END OF DUMP"
        echo "=========================================="
    } > "$dump_file" 2>&1

    echo "$dump_file"
    _e2e_verbose_write "FAILURE_DUMP created: $dump_file"
}
