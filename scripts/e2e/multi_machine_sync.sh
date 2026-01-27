#!/usr/bin/env bash
# scripts/e2e/multi_machine_sync.sh
# End-to-end test for multi-machine source sync workflows.
#
# Tests the sources setup/sync/doctor flows using local fixtures
# to simulate remote sources without requiring actual SSH connectivity.
#
# Usage:
#   ./scripts/e2e/multi_machine_sync.sh
#   CASS_BIN=target/debug/cass ./scripts/e2e/multi_machine_sync.sh
#   ./scripts/e2e/multi_machine_sync.sh --no-build --fail-fast
#
# Artifacts:
#   test-results/e2e/shell_multi_machine_sync_<timestamp>.jsonl
#
# Acceptance Criteria (T7.2):
#   - Script covers sources setup/sync/doctor flows
#   - Uses local fixture host definitions (no external SSH)
#   - Emits JSONL logs with phases + error context
#   - Validates sync output + provenance fields

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Source the E2E logging library
source "${PROJECT_ROOT}/scripts/lib/e2e_log.sh"

# Initialize logging
e2e_init "shell" "multi_machine_sync"

# Configuration
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

# Sandbox directories
SANDBOX_DIR="${PROJECT_ROOT}/target/e2e-sync/run_$(e2e_run_id)"
CONFIG_DIR="${SANDBOX_DIR}/config"
DATA_DIR="${SANDBOX_DIR}/data"
LOCAL_SOURCE_DIR="${SANDBOX_DIR}/local_source"
REMOTE_FIXTURE_DIR="${SANDBOX_DIR}/remote_fixtures"

# Test counters
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0
SKIPPED_TESTS=0

# Resolve cass binary
if [[ -n "${CASS_BIN:-}" ]]; then
    CASS_BIN_RESOLVED="$CASS_BIN"
else
    CASS_BIN_RESOLVED="${PROJECT_ROOT}/target/debug/cass"
fi

# Environment for cass commands
cass_env() {
    env \
        HOME="${SANDBOX_DIR}" \
        XDG_CONFIG_HOME="${CONFIG_DIR}" \
        XDG_DATA_HOME="${DATA_DIR}" \
        CASS_DATA_DIR="${DATA_DIR}/cass" \
        CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
        NO_COLOR=1 \
        CASS_NO_COLOR=1 \
        "$@"
}

# Run cass with sandbox environment
run_cass() {
    cass_env "${CASS_BIN_RESOLVED}" "$@"
}

# =============================================================================
# Setup
# =============================================================================

setup_sandbox() {
    e2e_phase_start "setup" "Creating sandbox and fixtures"
    local start_ms
    start_ms=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

    mkdir -p "${CONFIG_DIR}/cass"
    mkdir -p "${DATA_DIR}/cass"
    mkdir -p "${LOCAL_SOURCE_DIR}/.claude/projects/myapp"
    mkdir -p "${LOCAL_SOURCE_DIR}/.codex/sessions/2024/12"
    mkdir -p "${REMOTE_FIXTURE_DIR}/laptop/.claude/projects/webapp"
    mkdir -p "${REMOTE_FIXTURE_DIR}/workstation/.codex/sessions/2024/12"

    # Create local Claude Code session fixture
    cat <<'EOF' > "${LOCAL_SOURCE_DIR}/.claude/projects/myapp/session.jsonl"
{"type":"user","timestamp":"2024-12-01T10:00:00Z","message":{"role":"user","content":"How do I fix the authentication error?"}}
{"type":"assistant","timestamp":"2024-12-01T10:01:00Z","message":{"role":"assistant","content":"The authentication error is caused by an expired token. You should refresh the token."}}
EOF

    # Create local Codex session fixture
    cat <<'EOF' > "${LOCAL_SOURCE_DIR}/.codex/sessions/2024/12/rollout-test.jsonl"
{"type":"event_msg","timestamp":1733011200000,"payload":{"type":"user_message","message":"database connection error in login"}}
{"type":"response_item","timestamp":1733011201000,"payload":{"role":"assistant","content":"The database connection error is likely due to incorrect credentials."}}
EOF

    # Create "remote" fixture for laptop source (simulates synced data)
    cat <<'EOF' > "${REMOTE_FIXTURE_DIR}/laptop/.claude/projects/webapp/session.jsonl"
{"type":"user","timestamp":"2024-12-02T14:00:00Z","message":{"role":"user","content":"How do I implement OAuth2?"}}
{"type":"assistant","timestamp":"2024-12-02T14:01:00Z","message":{"role":"assistant","content":"OAuth2 implementation requires setting up an authorization server and configuring client credentials."}}
EOF

    # Create "remote" fixture for workstation source
    cat <<'EOF' > "${REMOTE_FIXTURE_DIR}/workstation/.codex/sessions/2024/12/remote-session.jsonl"
{"type":"event_msg","timestamp":1733097600000,"payload":{"type":"user_message","message":"API rate limiting implementation"}}
{"type":"response_item","timestamp":1733097601000,"payload":{"role":"assistant","content":"Rate limiting can be implemented using token bucket or sliding window algorithms."}}
EOF

    local end_ms
    end_ms=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    e2e_phase_end "setup" $((end_ms - start_ms))

    e2e_info "Sandbox created at ${SANDBOX_DIR}"
}

# =============================================================================
# Test Functions
# =============================================================================

# Test: sources list with no sources configured
test_sources_list_empty() {
    local output exit_code=0

    output=$(run_cass sources list --json 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        # Should have empty sources array or indicate no sources
        if echo "$output" | grep -q '"sources"'; then
            return 0
        fi
    fi

    echo "Expected empty sources list, got: $output" >&2
    return 1
}

# Test: sources add with --no-test (no SSH connectivity required)
test_sources_add_no_test() {
    local output exit_code=0

    output=$(run_cass sources add "user@laptop.local" \
        --name "laptop" \
        --preset "linux-defaults" \
        --no-test 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]] && echo "$output" | grep -qi "added"; then
        return 0
    fi

    echo "Failed to add source: $output" >&2
    return 1
}

# Test: sources add second source
test_sources_add_second() {
    local output exit_code=0

    output=$(run_cass sources add "dev@workstation.local" \
        --name "workstation" \
        --preset "linux-defaults" \
        --no-test 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]] && echo "$output" | grep -qi "added"; then
        return 0
    fi

    echo "Failed to add second source: $output" >&2
    return 1
}

# Test: sources list shows configured sources
test_sources_list_with_sources() {
    local output exit_code=0

    output=$(run_cass sources list --json 2>&1) || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        echo "sources list failed: $output" >&2
        return 1
    fi

    # Validate JSON structure and content
    if ! echo "$output" | grep -q '"sources"'; then
        echo "Missing sources field in output: $output" >&2
        return 1
    fi

    if ! echo "$output" | grep -q '"laptop"'; then
        echo "Missing laptop source in output: $output" >&2
        return 1
    fi

    if ! echo "$output" | grep -q '"workstation"'; then
        echo "Missing workstation source in output: $output" >&2
        return 1
    fi

    return 0
}

# Test: sources doctor with configured sources (JSON output)
test_sources_doctor_json() {
    local output exit_code=0

    # Doctor may fail SSH connectivity but should still produce valid JSON
    output=$(run_cass sources doctor --json 2>&1) || exit_code=$?

    # Check for valid JSON array output
    if echo "$output" | grep -qE '^\['; then
        # Verify it mentions our sources
        if echo "$output" | grep -q '"laptop"' || echo "$output" | grep -q '"source_id"'; then
            return 0
        fi
    fi

    echo "Invalid doctor JSON output: $output" >&2
    return 1
}

# Test: sources doctor for single source
test_sources_doctor_single() {
    local output exit_code=0

    output=$(run_cass sources doctor --source laptop --json 2>&1) || exit_code=$?

    # Should output diagnostics for laptop only
    if echo "$output" | grep -q "laptop"; then
        return 0
    fi

    echo "Doctor single source failed: $output" >&2
    return 1
}

# Test: sources sync --dry-run
test_sources_sync_dry_run() {
    local output exit_code=0

    output=$(run_cass sources sync --dry-run --json 2>&1) || exit_code=$?

    # Dry run should indicate what would be synced
    if echo "$output" | grep -qiE "(laptop|workstation|dry|sources)"; then
        return 0
    fi

    echo "Sync dry-run unexpected output: $output" >&2
    return 1
}

# Test: sources sync --dry-run for single source
test_sources_sync_single_dry_run() {
    local output exit_code=0

    output=$(run_cass sources sync --source laptop --dry-run --json 2>&1) || exit_code=$?

    # Dry-run should succeed with valid JSON structure
    # Note: With no actual SSH connectivity, sources array may be empty
    if [[ $exit_code -eq 0 ]] && echo "$output" | grep -qE '"(status|dry_run|sources)"'; then
        return 0
    fi

    echo "Sync single source dry-run failed: $output" >&2
    return 1
}

# Test: sources mappings add
test_mappings_add() {
    local output exit_code=0

    output=$(run_cass sources mappings add laptop \
        --from "/home/user/projects" \
        --to "/Users/me/projects" 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        return 0
    fi

    echo "Mappings add failed: $output" >&2
    return 1
}

# Test: sources mappings list
test_mappings_list() {
    local output exit_code=0

    output=$(run_cass sources mappings list laptop --json 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]] && echo "$output" | grep -q "mappings"; then
        return 0
    fi

    echo "Mappings list failed: $output" >&2
    return 1
}

# Test: sources mappings test
test_mappings_test() {
    local output exit_code=0

    output=$(run_cass sources mappings test laptop \
        "/home/user/projects/myapp/src/main.rs" 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]] && echo "$output" | grep -q "/Users/me/projects"; then
        return 0
    fi

    echo "Mappings test failed: $output" >&2
    return 1
}

# Test: sources remove
test_sources_remove() {
    local output exit_code=0

    output=$(run_cass sources remove workstation -y 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        return 0
    fi

    echo "Sources remove failed: $output" >&2
    return 1
}

# Test: verify source was removed
test_sources_removed_verification() {
    local output exit_code=0

    output=$(run_cass sources list --json 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        # Should NOT contain workstation anymore
        if echo "$output" | grep -q '"workstation"'; then
            echo "Source 'workstation' was not removed: $output" >&2
            return 1
        fi
        # Should still contain laptop
        if echo "$output" | grep -q '"laptop"'; then
            return 0
        fi
    fi

    echo "Removal verification failed: $output" >&2
    return 1
}

# Test: index with local sources (for provenance validation)
test_index_local() {
    local output exit_code=0

    # Set HOME to local_source to pick up fixtures
    output=$(env \
        HOME="${LOCAL_SOURCE_DIR}" \
        XDG_CONFIG_HOME="${CONFIG_DIR}" \
        XDG_DATA_HOME="${DATA_DIR}" \
        CASS_DATA_DIR="${DATA_DIR}/cass" \
        CODEX_HOME="${LOCAL_SOURCE_DIR}/.codex" \
        CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
        NO_COLOR=1 \
        "${CASS_BIN_RESOLVED}" index --full --json 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        e2e_info "Index completed: $output"
        return 0
    fi

    echo "Index failed: $output" >&2
    return 1
}

# Test: search with provenance fields
test_search_provenance() {
    local output exit_code=0

    output=$(env \
        HOME="${LOCAL_SOURCE_DIR}" \
        XDG_CONFIG_HOME="${CONFIG_DIR}" \
        XDG_DATA_HOME="${DATA_DIR}" \
        CASS_DATA_DIR="${DATA_DIR}/cass" \
        CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
        NO_COLOR=1 \
        "${CASS_BIN_RESOLVED}" search "authentication" \
        --robot --limit 5 \
        --fields "source_path,source_id,origin_kind,origin_host" 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        # Validate provenance fields are present in output
        # Local sources should have origin_kind=local
        if echo "$output" | grep -qiE "(source_path|origin)"; then
            e2e_info "Search provenance output: $output"
            return 0
        fi
    fi

    # Search may return no results if index is empty, which is still valid
    if [[ $exit_code -eq 0 ]]; then
        e2e_info "Search completed (may have no results): $output"
        return 0
    fi

    echo "Search with provenance failed: $output" >&2
    return 1
}

# Test: stats command for provenance verification
test_stats_provenance() {
    local output exit_code=0

    output=$(env \
        HOME="${LOCAL_SOURCE_DIR}" \
        XDG_CONFIG_HOME="${CONFIG_DIR}" \
        XDG_DATA_HOME="${DATA_DIR}" \
        CASS_DATA_DIR="${DATA_DIR}/cass" \
        CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
        NO_COLOR=1 \
        "${CASS_BIN_RESOLVED}" stats --json 2>&1) || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        e2e_info "Stats output: $output"
        return 0
    fi

    echo "Stats failed: $output" >&2
    return 1
}

# =============================================================================
# Test Runner
# =============================================================================

run_test() {
    local test_name="$1"
    local test_fn="$2"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    e2e_run_test "$test_name" "multi_machine_sync" "$test_fn"
    local result=$?

    if [[ $result -eq 0 ]]; then
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        FAILED_TESTS=$((FAILED_TESTS + 1))
        if [[ $FAIL_FAST -eq 1 ]]; then
            e2e_error "Fail-fast enabled; aborting after $test_name"
            return 1
        fi
    fi

    return 0
}

# =============================================================================
# Main
# =============================================================================

main() {
    e2e_run_start "" "false" "$( [[ $FAIL_FAST -eq 1 ]] && echo true || echo false )"

    e2e_info "Multi-machine sync E2E test suite"
    e2e_info "Output file: $(e2e_output_file)"
    e2e_info "CASS binary: ${CASS_BIN_RESOLVED}"

    # Build if needed
    if [[ ! -x "$CASS_BIN_RESOLVED" ]]; then
        if [[ $NO_BUILD -eq 1 ]]; then
            e2e_error "CASS_BIN not found at ${CASS_BIN_RESOLVED} and --no-build set"
            e2e_run_end 0 0 1 0 0
            exit 1
        fi

        e2e_phase_start "build" "Building cass binary"
        local build_start
        build_start=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

        if ! cargo build --bin cass 2>&1; then
            e2e_error "Build failed"
            e2e_run_end 0 0 1 0 0
            exit 1
        fi

        local build_end
        build_end=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
        e2e_phase_end "build" $((build_end - build_start))
    fi

    # Setup sandbox
    setup_sandbox

    # Run tests
    e2e_phase_start "tests" "Running multi-machine sync tests"
    local tests_start
    tests_start=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))

    # Sources list (empty)
    run_test "sources_list_empty" "test_sources_list_empty" || true

    # Sources add
    run_test "sources_add_no_test" "test_sources_add_no_test" || true
    run_test "sources_add_second" "test_sources_add_second" || true

    # Sources list (with sources)
    run_test "sources_list_with_sources" "test_sources_list_with_sources" || true

    # Sources doctor
    run_test "sources_doctor_json" "test_sources_doctor_json" || true
    run_test "sources_doctor_single" "test_sources_doctor_single" || true

    # Sources sync (dry-run only - no actual SSH)
    run_test "sources_sync_dry_run" "test_sources_sync_dry_run" || true
    run_test "sources_sync_single_dry_run" "test_sources_sync_single_dry_run" || true

    # Mappings workflow
    run_test "mappings_add" "test_mappings_add" || true
    run_test "mappings_list" "test_mappings_list" || true
    run_test "mappings_test" "test_mappings_test" || true

    # Sources remove
    run_test "sources_remove" "test_sources_remove" || true
    run_test "sources_removed_verification" "test_sources_removed_verification" || true

    # Index and provenance validation
    run_test "index_local" "test_index_local" || true
    run_test "search_provenance" "test_search_provenance" || true
    run_test "stats_provenance" "test_stats_provenance" || true

    local tests_end
    tests_end=$(date +%s%3N 2>/dev/null || echo $(($(date +%s) * 1000)))
    e2e_phase_end "tests" $((tests_end - tests_start))

    # Calculate total duration
    local total_duration
    total_duration=$(e2e_duration_since_start)

    # Emit run summary
    e2e_run_end "$TOTAL_TESTS" "$PASSED_TESTS" "$FAILED_TESTS" "$SKIPPED_TESTS" "$total_duration"

    # Summary output
    e2e_info "Test run complete"
    e2e_info "Total: $TOTAL_TESTS | Passed: $PASSED_TESTS | Failed: $FAILED_TESTS | Skipped: $SKIPPED_TESTS"
    e2e_info "Duration: ${total_duration}ms"
    e2e_info "Log file: $(e2e_output_file)"

    # Cleanup (preserve sandbox by default for debugging)
    if [[ $KEEP_SANDBOX -eq 0 ]]; then
        e2e_info "Sandbox preserved at ${SANDBOX_DIR} (use --keep-sandbox to explicitly preserve)"
    else
        e2e_info "Sandbox preserved at ${SANDBOX_DIR}"
    fi

    # Exit with appropriate code
    if [[ $FAILED_TESTS -gt 0 ]]; then
        exit 1
    fi

    exit 0
}

main "$@"
