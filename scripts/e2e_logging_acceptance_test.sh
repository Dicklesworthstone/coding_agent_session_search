#!/usr/bin/env bash
# Comprehensive E2E Logging Acceptance Test
#
# This test verifies the entire E2E logging system works end-to-end by running
# all E2E tests with logging enabled and validating the aggregated output.
#
# Usage:
#   RCH_WORKER=vmi1149989 ./scripts/e2e_logging_acceptance_test.sh
#   ./scripts/e2e_logging_acceptance_test.sh --quick --run-id <completed-run>
#
# Options:
#   --quick          Verify one explicit completed run without executing tests
#   --run-id ID      Collision-free run id (generated for a full run)
#   --worker ID      Exact RCH worker; defaults to RCH_WORKER
#
# Environment:
#   RCH_BIN               rch executable (default: rch)
#   RCH_TEST_TIMEOUT_SEC  strict remote test timeout (default: 7200)
#
# Exit codes:
#   0 - Acceptance test passed
#   1 - Acceptance test failed

set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"
RCH_BIN="${RCH_BIN:-rch}"
HANDOFF_TIMEOUT_SECONDS="${CASS_E2E_HANDOFF_TIMEOUT_SECONDS:-900}"

# Configuration
E2E_RESULTS_ROOT="test-results/e2e"
TEST_RESULTS_DIR=""
REPORT_FILE=""
TEST_OUTPUT_FILE=""
QUICK_MODE=false
PUBLISH_PENDING=false
CONTRACT_ONLY=false
CONCURRENCY_CONTRACT=false
CONTRACT_TARGET=""
CONTRACT_FILTER=""
EVIDENCE_SCOPE="comprehensive-e2e"
RUN_ID="${CASS_E2E_RUN_ID:-}"
WORKER_ID="${RCH_WORKER:-}"
SSH_TARGET=""
SSH_OPTIONS=(
    -o BatchMode=yes
    -o IdentitiesOnly=yes
    -o ConnectTimeout=15
    -o ConnectionAttempts=1
    -o ServerAliveInterval=15
    -o ServerAliveCountMax=4
)
RSYNC_RSH="ssh -o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=15 -o ConnectionAttempts=1 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --quick)
            QUICK_MODE=true
            shift
            ;;
        --run-id)
            [[ $# -ge 2 ]] || {
                echo "Error: --run-id requires a value"
                exit 2
            }
            RUN_ID="$2"
            shift 2
            ;;
        --worker)
            [[ $# -ge 2 ]] || {
                echo "Error: --worker requires a value"
                exit 2
            }
            WORKER_ID="$2"
            shift 2
            ;;
        --contract-only)
            CONTRACT_ONLY=true
            shift
            ;;
        --contract-target)
            [[ $# -ge 2 ]] || {
                echo "Error: --contract-target requires a value"
                exit 2
            }
            CONTRACT_TARGET="$2"
            shift 2
            ;;
        --contract-filter)
            [[ $# -ge 2 ]] || {
                echo "Error: --contract-filter requires a value"
                exit 2
            }
            CONTRACT_FILTER="$2"
            shift 2
            ;;
        --concurrency-contract)
            CONCURRENCY_CONTRACT=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [--quick] [--run-id ID] [--worker ID]"
            echo "       $0 --contract-only --contract-target TARGET --contract-filter FILTER [--run-id ID] --worker ID"
            echo "       $0 --concurrency-contract --worker ID"
            echo ""
            echo "Options:"
            echo "  --quick          Verify one explicit completed run without executing tests"
            echo "  --run-id ID      Collision-free run id (generated for a full run)"
            echo "  --worker ID      Exact RCH worker; defaults to RCH_WORKER"
            echo "  --contract-only  Run one explicit no-mock target/filter as bundle-contract evidence"
            echo "  --contract-target TARGET  Integration-test target for --contract-only"
            echo "  --contract-filter FILTER  Exact test filter for --contract-only"
            echo "  --concurrency-contract    Run two isolated contract bundles concurrently"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Ensure jq is available
if ! command -v jq &>/dev/null; then
    echo "Error: jq is required but not installed"
    exit 1
fi

validate_run_id() {
    local run_id="$1"
    if [[ ${#run_id} -lt 8 || ${#run_id} -gt 128 ]] \
        || [[ ! "$run_id" =~ ^[A-Za-z0-9][A-Za-z0-9_-]*[A-Za-z0-9]$ ]]; then
        echo "Error: run id must be 8-128 ASCII alphanumeric, '_' or '-' bytes" >&2
        return 2
    fi
}

validate_worker_id() {
    local worker_id="$1"
    if [[ -z "$worker_id" || ${#worker_id} -gt 64 ]] \
        || [[ ! "$worker_id" =~ ^[A-Za-z0-9]([A-Za-z0-9_-]*[A-Za-z0-9])?$ ]]; then
        echo "Error: worker id must be 1-64 ASCII alphanumeric, '_' or '-' bytes" >&2
        return 2
    fi
}

validate_contract_mode() {
    if [[ "$QUICK_MODE" == true && (
        "$CONTRACT_ONLY" == true
        || "$CONCURRENCY_CONTRACT" == true
        || -n "$CONTRACT_TARGET"
        || -n "$CONTRACT_FILTER"
    ) ]]; then
        echo "Error: --quick cannot be combined with contract execution modes"
        return 2
    fi
    if [[ "$CONCURRENCY_CONTRACT" == true ]]; then
        if [[ "$CONTRACT_ONLY" == true || -n "$CONTRACT_TARGET" || -n "$CONTRACT_FILTER" ]]; then
            echo "Error: --concurrency-contract selects its own fixed no-mock witness"
            return 2
        fi
        if [[ -n "$RUN_ID" ]]; then
            echo "Error: --concurrency-contract allocates two run IDs; do not provide CASS_E2E_RUN_ID or --run-id"
            return 2
        fi
    elif [[ "$CONTRACT_ONLY" == true ]]; then
        if [[ ! "$CONTRACT_TARGET" =~ ^[A-Za-z0-9_]+$ ]] \
            || [[ ! "$CONTRACT_FILTER" =~ ^[A-Za-z0-9_:]+$ ]]; then
            echo "Error: --contract-only requires shell-safe --contract-target and --contract-filter"
            return 2
        fi
        EVIDENCE_SCOPE="run-bundle-contract:$CONTRACT_TARGET::$CONTRACT_FILTER"
    elif [[ -n "$CONTRACT_TARGET" || -n "$CONTRACT_FILTER" ]]; then
        echo "Error: --contract-target/--contract-filter require --contract-only"
        return 2
    fi
}

ensure_rch() {
    if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
        echo "Error: rch is required for offloaded Cargo test execution"
        exit 1
    fi
    if [[ ! "$HANDOFF_TIMEOUT_SECONDS" =~ ^[1-9][0-9]{0,4}$ ]]; then
        echo "Error: CASS_E2E_HANDOFF_TIMEOUT_SECONDS must be 1-99999 seconds"
        exit 1
    fi
    for command in ssh rsync sha256sum timeout; do
        if ! command -v "$command" >/dev/null 2>&1; then
            echo "Error: $command is required for exact-worker artifact handoff"
            exit 1
        fi
    done
}

worker_ssh() {
    timeout --signal=TERM --kill-after=10 "$HANDOFF_TIMEOUT_SECONDS" \
        ssh "${SSH_OPTIONS[@]}" "$SSH_TARGET" "$@"
}

worker_rsync() {
    timeout --signal=TERM --kill-after=10 "$HANDOFF_TIMEOUT_SECONDS" \
        env RSYNC_RSH="$RSYNC_RSH" \
        rsync -a --protect-args -- "$@"
}

resolve_worker_ssh_target() {
    local workers_json discovery_json worker_count discovery_count
    local worker_host worker_user identity_file ssh_host
    workers_json=$("$RCH_BIN" --json workers list) || {
        echo "Error: unable to read the RCH worker inventory"
        return 1
    }
    worker_count=$(
        jq -r --arg worker_id "$WORKER_ID" \
            '[.data.workers[] | select(.id == $worker_id)] | length' \
            <<<"$workers_json"
    )
    if [[ "$worker_count" != 1 ]]; then
        echo "Error: expected exactly one configured RCH worker named $WORKER_ID; found $worker_count"
        return 1
    fi
    worker_host=$(
        jq -er --arg worker_id "$WORKER_ID" \
            '.data.workers[] | select(.id == $worker_id) | .host' \
            <<<"$workers_json"
    )
    worker_user=$(
        jq -er --arg worker_id "$WORKER_ID" \
            '.data.workers[] | select(.id == $worker_id) | .user' \
            <<<"$workers_json"
    )
    discovery_json=$("$RCH_BIN" --json workers discover) || {
        echo "Error: unable to read RCH's machine-readable SSH discovery data"
        return 1
    }
    discovery_count=$(
        jq -r --arg worker_host "$worker_host" \
            '[.data.discovered[] | select(
                .hostname == $worker_host
                and (.identity_file | type == "string")
            )] | length' \
            <<<"$discovery_json"
    )
    if [[ "$discovery_count" != 1 ]]; then
        echo "Error: expected one SSH identity for RCH worker $WORKER_ID; found $discovery_count"
        return 1
    fi
    identity_file=$(
        jq -er --arg worker_host "$worker_host" \
            '.data.discovered[] | select(
                .hostname == $worker_host
                and (.identity_file | type == "string")
            ) | .identity_file' \
            <<<"$discovery_json"
    )
    if [[ ${#worker_user} -gt 64 || ! "$worker_user" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ ]]; then
        echo "Error: configured RCH worker has an unsafe SSH user"
        return 1
    fi
    if [[ -z "$worker_host" || ${#worker_host} -gt 255 ]] \
        || [[ ! "$worker_host" =~ ^[A-Za-z0-9:.][A-Za-z0-9:.-]*$ ]]; then
        echo "Error: configured RCH worker has an unsafe SSH host"
        return 1
    fi
    if [[ ! "$identity_file" =~ ^/[A-Za-z0-9_./-]+$ ]] \
        || [[ "$identity_file" == *".."* ]] \
        || [[ ! -r "$identity_file" ]]; then
        echo "Error: configured RCH worker identity is unsafe or unreadable"
        return 1
    fi
    ssh_host="$worker_host"
    if [[ "$worker_host" == *:* ]]; then
        ssh_host="[$worker_host]"
    fi
    SSH_TARGET="$worker_user@$ssh_host"
    SSH_OPTIONS+=(-i "$identity_file")
    RSYNC_RSH="$RSYNC_RSH -i $identity_file"
}

verify_bundle_locally() {
    local bundle_root="$1"
    local expected_run_id="$2"
    local expected_worker_id="$3"
    local expected_worker_hostname="$4"
    local expected_source_sha="$5"
    local expected_source_diff_sha256="$6"
    local manifest_file="$bundle_root/manifest.json"
    local completion_file="$bundle_root/complete.json"
    local control_file expected_manifest_sha actual_manifest_sha
    local expected_command_sha actual_command_sha argument argument_bytes encoded artifact
    local relative expected_hash expected_bytes expected_events expected_schema
    local actual_hash actual_bytes actual_events actual_schema artifact_path
    local histogram_key histogram_count current_histogram_count declared_histogram_count=0
    local declared_count=0 actual_count=0 total_bytes=0 total_events=0
    local trace_files=0 trace_bytes=0 trace_events=0
    declare -A declared_hash=()
    declare -A declared_bytes=()
    declare -A declared_events=()
    declare -A declared_schema=()
    declare -A seen=()
    declare -A actual_histogram=()

    if [[ ! -d "$bundle_root" || -L "$bundle_root" ]]; then
        echo "Error: bundle root must be a real directory: $bundle_root"
        return 1
    fi
    if [[ ${bundle_root##*/} != "$expected_run_id" ]]; then
        echo "Error: bundle directory name does not match run id $expected_run_id"
        return 1
    fi
    for control_file in "$manifest_file" "$completion_file"; do
        if [[ ! -f "$control_file" || -L "$control_file" ]]; then
            echo "Error: missing or aliased bundle control file: $control_file"
            return 1
        fi
        if [[ $(stat -c '%h' "$control_file") != 1 ]]; then
            echo "Error: bundle control file has multiple hard links: $control_file"
            return 1
        fi
    done
    if ! jq -e \
        --arg run_id "$expected_run_id" \
        --arg worker_id "$expected_worker_id" \
        --arg worker_hostname "$expected_worker_hostname" \
        --arg source_sha "$expected_source_sha" \
        --arg source_diff_sha256 "$expected_source_diff_sha256" \
        '
        .schema_version == 1
        and .run_id == $run_id
        and .worker_id == $worker_id
        and .worker_hostname == $worker_hostname
        and .source_sha == $source_sha
        and .source_diff_sha256 == $source_diff_sha256
        and .complete == true
        and (.source_sha | test("^[0-9a-f]{40}$"))
        and (.source_diff_sha256 | test("^[0-9a-f]{64}$"))
        and (.command_sha256 | test("^[0-9a-f]{64}$"))
        and (.started_at_unix_ms | type == "number")
        and (.ended_at_unix_ms | type == "number")
        and .ended_at_unix_ms >= .started_at_unix_ms
        and (.cargo_exit_code | type == "number")
        and (.command | type == "array" and length >= 3)
        and all(.command[]; type == "string" and test("^[A-Za-z0-9_./:=+,@-]+$"))
        and .command[0] == "cargo"
        and .command[1] == "test"
        and (
            (.command | index("--locked")) as $locked
            | (.command | index("--test")) as $test
            | (.command | index("--")) as $separator
            | ($locked != null)
            and ($test != null)
            and (
                $separator == null
                or ($locked < $separator and $test < $separator)
            )
        )
        and ((.command | index("--no-run")) == null)
        and (.files | type == "array" and length > 0)
        and all(.files[];
            (.path | type == "string" and length > 0)
            and (.sha256 | test("^[0-9a-f]{64}$"))
            and (.bytes | type == "number" and . >= 0)
            and (.events | type == "number" and . >= 0)
            and (.schema | IN("raw", "jsonl", "e2e-log-v1", "cass-trace-v1"))
        )
        and (.gates | type == "array" and length > 0)
        and all(.gates[];
            (.name | type == "string" and length > 0)
            and (.passed | type == "boolean")
            and (.detail | type == "string" and length > 0)
        )
        and (([.gates[].name] | length) == ([.gates[].name] | unique | length))
        and ([
            "cargo-tests",
            "jsonl-schema",
            "bounded-capture-complete",
            "artifact-set-nonempty",
            "trace-artifacts-nonempty",
            "required-event-coverage"
        ] - [.gates[].name] | length == 0)
        ' \
        "$manifest_file" >/dev/null; then
        echo "Error: manifest shape, provenance, command, files, or gates are invalid"
        return 1
    fi

    expected_manifest_sha=$(jq -er '.manifest_sha256' "$completion_file")
    actual_manifest_sha=$(sha256sum "$manifest_file" | awk '{print $1}')
    if [[ "$expected_manifest_sha" != "$actual_manifest_sha" ]]; then
        echo "Error: completion receipt does not match the retrieved manifest"
        return 1
    fi
    if ! jq -e \
        --arg run_id "$expected_run_id" \
        --arg manifest_sha256 "$actual_manifest_sha" \
        --argjson ended_at "$(jq -er '.ended_at_unix_ms' "$manifest_file")" \
        '
        .schema_version == 1
        and .run_id == $run_id
        and .manifest_sha256 == $manifest_sha256
        and .finalized_at_unix_ms == $ended_at
        ' \
        "$completion_file" >/dev/null; then
        echo "Error: completion receipt fields do not bind the exact manifest"
        return 1
    fi

    expected_command_sha=$(jq -er '.command_sha256' "$manifest_file")
    actual_command_sha=$(
        {
            printf 'cass-e2e-command-v1\n'
            while IFS= read -r encoded; do
                argument=$(printf '%s' "$encoded" | base64 --decode)
                argument_bytes=$(printf '%s' "$argument" | wc -c | tr -d ' ')
                printf '%s:' "$argument_bytes"
                printf '%s' "$argument"
                printf '\n'
            done < <(jq -r '.command[] | @base64' "$manifest_file")
        } | sha256sum | awk '{print $1}'
    )
    if [[ "$expected_command_sha" != "$actual_command_sha" ]]; then
        echo "Error: command SHA-256 does not match the declared Cargo command"
        return 1
    fi

    while IFS= read -r encoded; do
        artifact=$(printf '%s' "$encoded" | base64 --decode)
        relative=$(jq -er '.path' <<<"$artifact")
        expected_hash=$(jq -er '.sha256' <<<"$artifact")
        expected_bytes=$(jq -er '.bytes' <<<"$artifact")
        expected_events=$(jq -er '.events' <<<"$artifact")
        expected_schema=$(jq -er '.schema' <<<"$artifact")
        if [[ ! "$relative" =~ ^[A-Za-z0-9_./-]+$ ]] \
            || [[ "$relative" == /* || "$relative" == *".."* || "$relative" == *"//"* ]] \
            || [[ "$relative" == "manifest.json" || "$relative" == "complete.json" ]]; then
            echo "Error: unsafe declared artifact path: $relative"
            return 1
        fi
        if [[ -n "${declared_hash[$relative]+present}" ]]; then
            echo "Error: duplicate declared artifact path: $relative"
            return 1
        fi
        declared_hash["$relative"]="$expected_hash"
        declared_bytes["$relative"]="$expected_bytes"
        declared_events["$relative"]="$expected_events"
        declared_schema["$relative"]="$expected_schema"
        declared_count=$((declared_count + 1))
    done < <(jq -r '.files[] | @base64' "$manifest_file")

    while IFS= read -r -d '' artifact_path; do
        if [[ -L "$artifact_path" ]]; then
            echo "Error: symbolic links are forbidden in a run bundle: $artifact_path"
            return 1
        fi
        if [[ -d "$artifact_path" ]]; then
            continue
        fi
        if [[ ! -f "$artifact_path" ]]; then
            echo "Error: non-regular bundle entry is forbidden: $artifact_path"
            return 1
        fi
        if [[ $(stat -c '%h' "$artifact_path") != 1 ]]; then
            echo "Error: bundle artifact has multiple hard links: $artifact_path"
            return 1
        fi
        relative=${artifact_path#"$bundle_root"/}
        if [[ "$relative" == "manifest.json" || "$relative" == "complete.json" ]]; then
            continue
        fi
        if [[ -z "${declared_hash[$relative]+present}" ]]; then
            echo "Error: bundle contains undeclared artifact: $relative"
            return 1
        fi
        actual_hash=$(sha256sum "$artifact_path" | awk '{print $1}')
        actual_bytes=$(stat -c '%s' "$artifact_path")
        if [[ "$actual_hash" != "${declared_hash[$relative]}" ]] \
            || [[ "$actual_bytes" != "${declared_bytes[$relative]}" ]]; then
            echo "Error: artifact byte identity mismatch: $relative"
            return 1
        fi

        if [[ "$relative" == *.jsonl || "${relative##*/}" == "cass.log" ]]; then
            if ! jq -e -s '.' "$artifact_path" >/dev/null; then
                echo "Error: invalid JSONL artifact: $relative"
                return 1
            fi
            actual_events=$(jq -s 'length' "$artifact_path")
            actual_schema=$(
                jq -s -r '
                    [
                        .[] |
                        if .schema_version == "cass-trace-v1" then
                            "cass-trace-v1"
                        elif (.event | type) == "string" then
                            "e2e-log-v1"
                        else
                            "jsonl"
                        end
                    ] | unique |
                    if length == 0 then "empty"
                    elif length == 1 then .[0]
                    else "mixed"
                    end
                ' \
                    "$artifact_path"
            )
            if [[ "$actual_schema" == "empty" ]]; then
                if [[ "${relative##*/}" == "trace.jsonl" ]]; then
                    actual_schema="cass-trace-v1"
                else
                    actual_schema="jsonl"
                fi
            fi
            if [[ "$actual_schema" == "mixed" ]]; then
                echo "Error: JSONL artifact mixes incompatible schemas: $relative"
                return 1
            fi
            while IFS= read -r encoded; do
                artifact=$(printf '%s' "$encoded" | base64 --decode)
                histogram_key=$(jq -er '.key' <<<"$artifact")
                histogram_count=$(jq -er '.value' <<<"$artifact")
                if [[ -z "$histogram_key" ]]; then
                    echo "Error: artifact contains an empty event name: $relative"
                    return 1
                fi
                current_histogram_count=${actual_histogram["$histogram_key"]:-0}
                actual_histogram["$histogram_key"]=$((current_histogram_count + histogram_count))
            done < <(
                jq -s -r '
                    reduce .[] as $record ({};
                        (
                            if $record.schema_version == "cass-trace-v1" then
                                ($record.fields.event // $record.fields.message // null)
                            elif ($record.event | type) == "string" then
                                $record.event
                            else
                                null
                            end
                        ) as $event |
                        if ($event | type) == "string" then
                            .[$event] = ((.[$event] // 0) + 1)
                        else
                            .
                        end
                    ) | to_entries[] | @base64
                ' \
                    "$artifact_path"
            )
        else
            actual_events=0
            actual_schema="raw"
        fi
        if [[ "$actual_events" != "${declared_events[$relative]}" ]] \
            || [[ "$actual_schema" != "${declared_schema[$relative]}" ]]; then
            echo "Error: artifact event count or schema mismatch: $relative"
            return 1
        fi
        seen["$relative"]=1
        actual_count=$((actual_count + 1))
        total_bytes=$((total_bytes + actual_bytes))
        total_events=$((total_events + actual_events))
        if [[ "$actual_schema" == "cass-trace-v1" ]]; then
            trace_files=$((trace_files + 1))
            trace_bytes=$((trace_bytes + actual_bytes))
            trace_events=$((trace_events + actual_events))
        fi
    done < <(find -P "$bundle_root" -mindepth 1 -print0)

    if [[ "$actual_count" -ne "$declared_count" ]]; then
        echo "Error: declared and actual artifact counts differ"
        return 1
    fi
    for relative in "${!declared_hash[@]}"; do
        if [[ -z "${seen[$relative]+present}" ]]; then
            echo "Error: declared artifact is missing: $relative"
            return 1
        fi
    done
    if ! jq -e \
        --argjson files "$actual_count" \
        --argjson bytes "$total_bytes" \
        --argjson events "$total_events" \
        --argjson trace_files "$trace_files" \
        --argjson trace_bytes "$trace_bytes" \
        --argjson trace_events "$trace_events" \
        '
        .aggregates.files == $files
        and .aggregates.bytes == $bytes
        and .aggregates.events == $events
        and .aggregates.trace_files == $trace_files
        and .aggregates.trace_bytes == $trace_bytes
        and .aggregates.trace_events == $trace_events
        ' \
        "$manifest_file" >/dev/null; then
        echo "Error: manifest aggregate counts do not match retrieved artifacts"
        return 1
    fi
    while IFS= read -r encoded; do
        artifact=$(printf '%s' "$encoded" | base64 --decode)
        histogram_key=$(jq -er '.key' <<<"$artifact")
        histogram_count=$(jq -er '.value' <<<"$artifact")
        if [[ "${actual_histogram[$histogram_key]:-0}" != "$histogram_count" ]]; then
            echo "Error: manifest event histogram mismatch for $histogram_key"
            return 1
        fi
        declared_histogram_count=$((declared_histogram_count + 1))
    done < <(jq -r '.aggregates.event_histogram | to_entries[] | @base64' "$manifest_file")
    if [[ "$declared_histogram_count" -ne "${#actual_histogram[@]}" ]]; then
        echo "Error: manifest event histogram key set does not match artifacts"
        return 1
    fi
}

run_cargo() {
    RCH_WORKER="$WORKER_ID" \
    RCH_REQUIRE_REMOTE=1 \
        RCH_NO_SELF_HEALING=1 \
        RCH_TEST_TIMEOUT_SEC="${RCH_TEST_TIMEOUT_SEC:-7200}" \
        env -u CARGO_TARGET_DIR \
        "$RCH_BIN" --no-self-healing exec -- cargo "$@"
}

run_concurrency_contract() {
    local nonce timestamp stale_id left_id right_id stale_root remote_stale_root contract_root
    local left_log right_log left_pid right_pid left_exit right_exit manifest
    if [[ -z "$WORKER_ID" ]]; then
        echo "Error: --concurrency-contract requires RCH_WORKER or --worker"
        return 2
    fi
    validate_worker_id "$WORKER_ID"
    ensure_rch
    resolve_worker_ssh_target
    timestamp=$(date -u +%Y%m%dT%H%M%SZ)
    nonce=$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
    stale_id="contract-stale-$timestamp-$nonce"
    left_id="contract-left-$timestamp-$nonce"
    right_id="contract-right-$timestamp-$nonce"
    stale_root="$E2E_RESULTS_ROOT/runs/$stale_id"
    contract_root="$E2E_RESULTS_ROOT/contract-concurrency/$timestamp-$nonce"
    mkdir -p "$E2E_RESULTS_ROOT/runs" "$contract_root"
    mkdir "$stale_root"
    printf '%s\n' \
        "{\"schema_version\":1,\"run_id\":\"$stale_id\",\"complete\":true,\"cargo_exit_code\":0,\"gates\":[{\"name\":\"stale-pass\",\"passed\":true,\"detail\":\"must never be consumed\"}]}" \
        >"$stale_root/manifest.json"
    printf '%s\n' \
        "{\"ts\":\"2026-01-01T00:00:00Z\",\"event\":\"run_start\",\"run_id\":\"$stale_id\",\"runner\":\"stale\"}" \
        "{\"ts\":\"2026-01-01T00:00:01Z\",\"event\":\"test_start\",\"run_id\":\"$stale_id\",\"runner\":\"stale\"}" \
        "{\"ts\":\"2026-01-01T00:00:02Z\",\"event\":\"test_end\",\"run_id\":\"$stale_id\",\"runner\":\"stale\",\"result\":{\"status\":\"pass\",\"duration_ms\":1}}" \
        "{\"ts\":\"2026-01-01T00:00:03Z\",\"event\":\"run_end\",\"run_id\":\"$stale_id\",\"runner\":\"stale\",\"summary\":{},\"exit_code\":0}" \
        >"$stale_root/run.jsonl"
    if [[ ! "$PROJECT_ROOT" =~ ^/[A-Za-z0-9_./-]+$ || "$PROJECT_ROOT" == *".."* ]]; then
        echo "Error: project root is unsafe for exact-worker stale-evidence setup"
        return 1
    fi
    remote_stale_root="$PROJECT_ROOT/$stale_root"
    if ! worker_ssh \
        "test -d '$PROJECT_ROOT' && test ! -e '$remote_stale_root' && mkdir -p '$remote_stale_root'"; then
        echo "Error: could not create contradictory stale evidence on exact worker $WORKER_ID"
        return 1
    fi
    if ! worker_rsync "$stale_root/" "$SSH_TARGET:$remote_stale_root/"; then
        echo "Error: could not transfer contradictory stale evidence to exact worker $WORKER_ID"
        return 1
    fi
    if ! worker_ssh \
        "test -f '$remote_stale_root/manifest.json' && test -f '$remote_stale_root/run.jsonl'"; then
        echo "Error: contradictory remote stale evidence was not durably staged"
        return 1
    fi

    left_log="$contract_root/left.log"
    right_log="$contract_root/right.log"
    "$SCRIPT_DIR/e2e_logging_acceptance_test.sh" \
        --contract-only \
        --contract-target e2e_semantic_search \
        --contract-filter parallel_cass_children_keep_corpus_and_trace_artifacts_isolated \
        --run-id "$left_id" \
        --worker "$WORKER_ID" \
        >"$left_log" 2>&1 &
    left_pid=$!
    "$SCRIPT_DIR/e2e_logging_acceptance_test.sh" \
        --contract-only \
        --contract-target e2e_semantic_search \
        --contract-filter parallel_cass_children_keep_corpus_and_trace_artifacts_isolated \
        --run-id "$right_id" \
        --worker "$WORKER_ID" \
        >"$right_log" 2>&1 &
    right_pid=$!
    set +e
    wait "$left_pid"
    left_exit=$?
    wait "$right_pid"
    right_exit=$?
    set -e
    if [[ "$left_exit" -ne 0 || "$right_exit" -ne 0 ]]; then
        echo "Error: concurrent contract runs failed (left=$left_exit right=$right_exit)"
        echo "  Left log: $left_log"
        echo "  Right log: $right_log"
        echo "  Contradictory stale evidence retained at: $stale_root"
        return 1
    fi

    for manifest in \
        "$E2E_RESULTS_ROOT/runs/$left_id/manifest.json" \
        "$E2E_RESULTS_ROOT/runs/$right_id/manifest.json"; do
        if ! jq -e \
            --arg worker_id "$WORKER_ID" \
            --arg stale_id "$stale_id" \
            '
            .worker_id == $worker_id
            and .complete == true
            and .cargo_exit_code == 0
            and ([.gates[].passed] | all)
            and all(.files[]; (.path | contains($stale_id) | not))
            ' \
            "$manifest" >/dev/null; then
            echo "Error: concurrent bundle is not isolated or is not an acceptance pass: $manifest"
            return 1
        fi
    done
    if [[ $(jq -r '.run_id' "$E2E_RESULTS_ROOT/runs/$left_id/manifest.json") != "$left_id" ]] \
        || [[ $(jq -r '.run_id' "$E2E_RESULTS_ROOT/runs/$right_id/manifest.json") != "$right_id" ]]; then
        echo "Error: concurrent reports crossed run identities"
        return 1
    fi
    if grep -Fq "$stale_id" \
        "$E2E_RESULTS_ROOT/reports/$left_id.txt" \
        "$E2E_RESULTS_ROOT/reports/$right_id.txt"; then
        echo "Error: a concurrent report consumed contradictory stale evidence"
        return 1
    fi
    if ! worker_ssh \
        "test -f '$remote_stale_root/manifest.json' && test -f '$remote_stale_root/run.jsonl'"; then
        echo "Error: remote stale witness disappeared during concurrent exact-worker runs"
        return 1
    fi
    echo "Concurrent strict-RCH bundle contract passed"
    echo "  Left run: $left_id"
    echo "  Right run: $right_id"
    echo "  Worker: $WORKER_ID"
    echo "  Contradictory stale evidence retained and ignored: $stale_root"
    echo "  Contradictory remote stale evidence retained and ignored: $SSH_TARGET:$remote_stale_root"
    echo "  Detailed logs: $left_log $right_log"
}

validate_contract_mode
if [[ "$CONCURRENCY_CONTRACT" == true ]]; then
    run_concurrency_contract
    exit $?
fi

echo "=== E2E Logging Acceptance Test ==="
echo "This test verifies the entire E2E logging system works correctly."
echo ""

# Step 1: execute and retrieve one exact run, or re-verify one named run.
if [[ -z "$RUN_ID" && "$QUICK_MODE" == false ]]; then
    RUN_ID="acceptance-$(date -u +%Y%m%dT%H%M%SZ)-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
fi
if [[ -z "$RUN_ID" ]]; then
    echo "Error: --quick requires --run-id; arbitrary existing logs are never accepted"
    exit 2
fi
validate_run_id "$RUN_ID"

SOURCE_SHA=""
SOURCE_DIFF_SHA256=""
if [[ "$QUICK_MODE" == false ]]; then
    SOURCE_SHA=$(git rev-parse HEAD)
    SOURCE_DIFF_SHA256=$(
        git diff --binary HEAD -- . ':(exclude).beads/**' \
            | sha256sum \
            | awk '{print $1}'
    )
    if ! git diff --quiet HEAD -- . ':(exclude).beads/**'; then
        echo "Error: full acceptance requires committed source; tracker-only .beads changes are allowed"
        exit 1
    fi
    while IFS= read -r -d '' untracked; do
        case "$untracked" in
            .beads/*|test-results/*) ;;
            *)
                echo "Error: untracked source is not part of the exact source receipt: $untracked"
                exit 1
                ;;
        esac
    done < <(git ls-files --others --exclude-standard -z)
fi

FINAL_RUN_ROOT="$E2E_RESULTS_ROOT/runs/$RUN_ID"
mkdir -p "$E2E_RESULTS_ROOT/runs" "$E2E_RESULTS_ROOT/reports"

if [[ "$QUICK_MODE" == false ]]; then
    echo "Step 1: Running E2E tests into an immutable strict-RCH bundle..."
    if [[ -z "$WORKER_ID" ]]; then
        echo "Error: RCH_WORKER or --worker is required; full acceptance never auto-selects"
        exit 2
    fi
    validate_worker_id "$WORKER_ID"
    if [[ -e "$FINAL_RUN_ROOT" ]]; then
        echo "Error: refusing to reuse existing run destination: $FINAL_RUN_ROOT"
        exit 1
    fi
    ensure_rch
    resolve_worker_ssh_target

    E2E_TEST_ARGS=()
    if [[ "$CONTRACT_ONLY" == true ]]; then
        E2E_TEST_ARGS=(--test "$CONTRACT_TARGET" "$CONTRACT_FILTER")
    else
        while IFS= read -r test_file; do
            test_target=$(basename "$test_file" .rs)
            E2E_TEST_ARGS+=(--test "$test_target")
        done < <(find tests -maxdepth 1 -type f -name 'e2e_*.rs' | sort)
    fi
    if [[ ${#E2E_TEST_ARGS[@]} -eq 0 ]]; then
        echo "FAIL: No tests/e2e_*.rs targets were discovered"
        exit 1
    fi
    echo "  Run ID: $RUN_ID"
    echo "  Worker: $WORKER_ID"
    echo "  Source: $SOURCE_SHA"
    echo "  Source diff SHA-256: $SOURCE_DIFF_SHA256"
    echo "  Evidence scope: $EVIDENCE_SCOPE"
    if [[ "$CONTRACT_ONLY" == true ]]; then
        echo "  Contract target/filter: $CONTRACT_TARGET :: $CONTRACT_FILTER"
    else
        echo "  Explicit E2E targets: $((${#E2E_TEST_ARGS[@]} / 2))"
    fi

    mkdir -p "$E2E_RESULTS_ROOT/rch-streams"
    TEST_OUTPUT_FILE="$E2E_RESULTS_ROOT/rch-streams/$RUN_ID.log"
    set +e
    run_cargo run --locked --bin e2e-run-bundle -- \
        run \
        --run-id "$RUN_ID" \
        --worker-id "$WORKER_ID" \
        --source-sha "$SOURCE_SHA" \
        --source-diff-sha256 "$SOURCE_DIFF_SHA256" \
        --timeout-seconds "${RCH_TEST_TIMEOUT_SEC:-7200}" \
        -- \
        --locked \
        "${E2E_TEST_ARGS[@]}" \
        -- \
        --test-threads=1 2>&1 | tee "$TEST_OUTPUT_FILE"
    RCH_EXIT=$?
    set -e

    mapfile -t RUN_STARTS < <(
        jq -Rrc --arg run_id "$RUN_ID" \
            'fromjson? | select(
                .schema_version == 1
                and .kind == "e2e-run-started"
                and .run_id == $run_id
                and (.run_root | type == "string")
            )' \
            "$TEST_OUTPUT_FILE"
    )
    mapfile -t RUN_SUMMARIES < <(
        jq -Rrc --arg run_id "$RUN_ID" \
            'fromjson? | select(
                .schema_version == 1
                and .kind == "e2e-run-finalized"
                and .run_id == $run_id
                and (.manifest | type == "string")
            )' \
            "$TEST_OUTPUT_FILE"
    )
    if [[ ${#RUN_SUMMARIES[@]} -ne 1 ]]; then
        echo "Error: expected one versioned runner receipt for $RUN_ID; found ${#RUN_SUMMARIES[@]}"
        if [[ ${#RUN_STARTS[@]} -eq 1 ]]; then
            INCOMPLETE_REMOTE_ROOT=$(jq -er '.run_root' <<<"${RUN_STARTS[0]}")
            if [[ "$INCOMPLETE_REMOTE_ROOT" =~ ^/[A-Za-z0-9_./-]+$ ]] \
                && [[ "$INCOMPLETE_REMOTE_ROOT" != *".."* ]] \
                && [[ "$INCOMPLETE_REMOTE_ROOT" == */test-results/e2e/runs/"$RUN_ID" ]] \
                && worker_ssh "test -d '$INCOMPLETE_REMOTE_ROOT'"; then
                INCOMPLETE_STAGING_PARENT="$E2E_RESULTS_ROOT/incoming/$(date -u +%Y%m%dT%H%M%SZ)-$$"
                INCOMPLETE_STAGING_DIR="$INCOMPLETE_STAGING_PARENT/$RUN_ID"
                mkdir -p "$INCOMPLETE_STAGING_DIR"
                worker_rsync \
                    "$SSH_TARGET:$INCOMPLETE_REMOTE_ROOT/" \
                    "$INCOMPLETE_STAGING_DIR/"
                echo "  Incomplete remote run retained for diagnosis: $INCOMPLETE_STAGING_DIR"
            else
                echo "  Started receipt did not name one retrievable, validated remote run root"
            fi
        else
            echo "  Expected one versioned started receipt; found ${#RUN_STARTS[@]}"
        fi
        exit 1
    fi
    REMOTE_MANIFEST=$(jq -er '.manifest' <<<"${RUN_SUMMARIES[0]}")
    REMOTE_RUN_ROOT=${REMOTE_MANIFEST%/manifest.json}
    if [[ "$REMOTE_RUN_ROOT" == "$REMOTE_MANIFEST" ]] \
        || [[ ! "$REMOTE_RUN_ROOT" =~ ^/[A-Za-z0-9_./-]+$ ]] \
        || [[ "$REMOTE_RUN_ROOT" == *".."* ]] \
        || [[ "$REMOTE_RUN_ROOT" != */test-results/e2e/runs/"$RUN_ID" ]]; then
        echo "Error: runner receipt contains an unsafe or mismatched manifest path"
        exit 1
    fi
    if ! worker_ssh \
        "test -d '$REMOTE_RUN_ROOT' && test -f '$REMOTE_MANIFEST'"; then
        echo "Error: exact RCH worker does not contain the receipted bundle"
        exit 1
    fi
    REMOTE_HOSTNAME=$(worker_ssh hostname | tr -d '\r\n')
    if [[ -z "$REMOTE_HOSTNAME" ]]; then
        echo "Error: exact worker returned an empty hostname"
        exit 1
    fi

    STAGING_PARENT="$E2E_RESULTS_ROOT/incoming/$(date -u +%Y%m%dT%H%M%SZ)-$$"
    STAGING_DIR="$STAGING_PARENT/$RUN_ID"
    mkdir -p "$STAGING_DIR"
    worker_rsync \
        "$SSH_TARGET:$REMOTE_RUN_ROOT/" \
        "$STAGING_DIR/"

    if ! verify_bundle_locally \
        "$STAGING_DIR" \
        "$RUN_ID" \
        "$WORKER_ID" \
        "$REMOTE_HOSTNAME" \
        "$SOURCE_SHA" \
        "$SOURCE_DIFF_SHA256"; then
        echo "Error: retrieved run failed byte/provenance verification"
        echo "  Recoverable staging evidence retained at: $STAGING_DIR"
        exit 1
    fi
    CARGO_EXIT=$(jq -er '.cargo_exit_code' "$STAGING_DIR/manifest.json")
    ACCEPTANCE_PASS=$(jq -er '.complete and (.cargo_exit_code == 0) and ([.gates[].passed] | all)' \
        "$STAGING_DIR/manifest.json")
    if [[ $RCH_EXIT -ne 0 || "$ACCEPTANCE_PASS" != true ]]; then
        echo "Error: run failed closed (rch=$RCH_EXIT cargo=$CARGO_EXIT acceptance=$ACCEPTANCE_PASS)"
        echo "  Recoverable staging evidence retained at: $STAGING_DIR"
        if [[ "$CARGO_EXIT" -ne 0 ]]; then
            exit "$CARGO_EXIT"
        fi
        exit 1
    fi
    TEST_RESULTS_DIR="$STAGING_DIR"
    PUBLISH_PENDING=true
    echo "  Verified staged run bundle: $TEST_RESULTS_DIR"
    TEST_EXIT=0
else
    echo "Step 1: Quick mode - verifying only explicit run $RUN_ID..."
    EVIDENCE_SCOPE="sealed-run-reverification"
    if [[ ! -d "$FINAL_RUN_ROOT" ]]; then
        echo "Error: completed run directory does not exist: $FINAL_RUN_ROOT"
        exit 1
    fi
    SOURCE_SHA=$(jq -er '.source_sha' "$FINAL_RUN_ROOT/manifest.json")
    SOURCE_DIFF_SHA256=$(jq -er '.source_diff_sha256' "$FINAL_RUN_ROOT/manifest.json")
    if [[ -z "$WORKER_ID" ]]; then
        WORKER_ID=$(jq -er '.worker_id' "$FINAL_RUN_ROOT/manifest.json")
    fi
    validate_worker_id "$WORKER_ID"
    REMOTE_HOSTNAME=$(jq -er '.worker_hostname' "$FINAL_RUN_ROOT/manifest.json")
    verify_bundle_locally \
        "$FINAL_RUN_ROOT" \
        "$RUN_ID" \
        "$WORKER_ID" \
        "$REMOTE_HOSTNAME" \
        "$SOURCE_SHA" \
        "$SOURCE_DIFF_SHA256"
    ACCEPTANCE_PASS=$(jq -er '.complete and (.cargo_exit_code == 0) and ([.gates[].passed] | all)' \
        "$FINAL_RUN_ROOT/manifest.json")
    if [[ "$ACCEPTANCE_PASS" != true ]]; then
        echo "Error: completed bundle is integrity-valid but not an acceptance pass"
        exit 1
    fi
    TEST_RESULTS_DIR="$FINAL_RUN_ROOT"
    TEST_EXIT=0
fi

REPORT_FILE="$E2E_RESULTS_ROOT/reports/$RUN_ID.txt"

# Step 2: Verify JSONL files were created
echo ""
echo "Step 2: Verifying JSONL files created..."
JSONL_FILES=()
TRACE_FILES=()
while IFS= read -r -d '' file; do
    JSONL_FILES+=("$file")
done < <(
    find "$TEST_RESULTS_DIR" \
        -type f \( -name "*.jsonl" -o -name "cass.log" \) \
        ! -name "trace.jsonl" \
        ! -name "combined.jsonl" \
        ! -path "$TEST_RESULTS_DIR/.previous/*" \
        -print0 2>/dev/null | sort -z
)
while IFS= read -r -d '' file; do
    TRACE_FILES+=("$file")
done < <(
    find "$TEST_RESULTS_DIR" \
        -type f -name "trace.jsonl" \
        ! -path "$TEST_RESULTS_DIR/.previous/*" \
        -print0 2>/dev/null | sort -z
)

JSONL_COUNT=${#JSONL_FILES[@]}
if [[ "$JSONL_COUNT" -eq 0 ]]; then
    echo "FAIL: No JSONL files created in $TEST_RESULTS_DIR"
    echo "  Make sure E2E_LOG=1 is set when running tests"
    exit 1
fi
echo "  Found $JSONL_COUNT JSONL/log files"
echo "  Found ${#TRACE_FILES[@]} versioned per-test trace files"

NONEMPTY_TRACE_COUNT=0
TRACE_BYTES=0
TRACE_EVENTS=0
for trace_file in "${TRACE_FILES[@]}"; do
    if [[ -s "$trace_file" ]]; then
        NONEMPTY_TRACE_COUNT=$((NONEMPTY_TRACE_COUNT + 1))
        TRACE_BYTES=$((TRACE_BYTES + $(wc -c < "$trace_file")))
        TRACE_EVENTS=$((TRACE_EVENTS + $(wc -l < "$trace_file")))
    fi
done
echo "  Non-empty trace files: $NONEMPTY_TRACE_COUNT"
echo "  Persisted trace bytes: $TRACE_BYTES"
echo "  Persisted trace events: $TRACE_EVENTS"
if [[ "$QUICK_MODE" == false && "$NONEMPTY_TRACE_COUNT" -eq 0 ]]; then
    echo "FAIL: Full acceptance run produced no diagnostic trace events"
    exit 1
fi

# Step 3: Validate all JSONL files with schema validator
echo ""
echo "Step 3: Validating JSONL schema..."
if [[ -x "$SCRIPT_DIR/validate-e2e-jsonl.sh" ]]; then
    if ! "$SCRIPT_DIR/validate-e2e-jsonl.sh" "${JSONL_FILES[@]}" "${TRACE_FILES[@]}"; then
        echo "FAIL: JSONL schema validation failed"
        exit 1
    fi
    echo "  Schema validation passed"
else
    echo "  Warning: validate-e2e-jsonl.sh not found, skipping schema validation"
fi

if [[ "$NONEMPTY_TRACE_COUNT" -gt 0 ]]; then
    echo "  Trace target histogram (top 20):"
    jq -r '.target // "unknown"' "${TRACE_FILES[@]}" 2>/dev/null \
        | sort \
        | uniq -c \
        | sort -nr \
        | head -20 \
        | sed 's/^/    /'
    echo "  Trace receipts:"
    jq -r '.fields.event // empty' "${TRACE_FILES[@]}" 2>/dev/null \
        | awk '
            $0 == "trace_truncated" { truncated += 1 }
            $0 == "trace_filter_summary" { filtered += 1 }
            $0 == "command_summary" { commands += 1 }
            END {
                printf "    command_summary=%d trace_filter_summary=%d trace_truncated=%d\n",
                    commands, filtered, truncated
            }
        '
fi

# Step 4: Check event type coverage
echo ""
echo "Step 4: Checking event coverage..."
EVENTS=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq -r '.event' 2>/dev/null | sort -u | grep -v '^$' || true)

# In quick mode, only require test_start/test_end (per-test logs don't have run events)
# In full mode, require all event types including run_start/run_end
if [[ "$QUICK_MODE" == true ]]; then
    REQUIRED_EVENTS="test_start test_end"
    echo "  (Quick mode: only checking for test events)"
else
    REQUIRED_EVENTS="run_start test_start test_end run_end"
fi
MISSING_EVENTS=()

for event in $REQUIRED_EVENTS; do
    if ! echo "$EVENTS" | grep -q "^${event}$"; then
        MISSING_EVENTS+=("$event")
    fi
done

if [[ ${#MISSING_EVENTS[@]} -gt 0 ]]; then
    echo "FAIL: Missing required event types: ${MISSING_EVENTS[*]}"
    exit 1
fi
echo "  All required event types present: $REQUIRED_EVENTS"

# List all event types found
UNIQUE_EVENT_COUNT=$(echo "$EVENTS" | wc -l | tr -d ' ')
echo "  Found $UNIQUE_EVENT_COUNT unique event types: $(echo "$EVENTS" | tr '\n' ' ')"

# Step 5: Check phase event coverage
echo ""
echo "Step 5: Checking phase event coverage..."
PHASE_START_COUNT=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq 'select(.event == "phase_start")' 2>/dev/null | wc -l | tr -d ' ')
PHASE_END_COUNT=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq 'select(.event == "phase_end")' 2>/dev/null | wc -l | tr -d ' ')

echo "  phase_start events: $PHASE_START_COUNT"
echo "  phase_end events: $PHASE_END_COUNT"

PHASE_WARNING=""
if [[ "$PHASE_END_COUNT" -lt 10 ]]; then
    PHASE_WARNING="WARNING: Only $PHASE_END_COUNT phase_end events (target: > 10)"
    echo "  $PHASE_WARNING"
else
    echo "  Phase coverage: OK (> 10 phase events)"
fi

# Step 6: Check metrics event coverage
echo ""
echo "Step 6: Checking metrics coverage..."
METRICS_COUNT=$(cat "${JSONL_FILES[@]}" 2>/dev/null | jq 'select(.event == "metrics")' 2>/dev/null | wc -l | tr -d ' ')
echo "  metrics events: $METRICS_COUNT"

METRICS_WARNING=""
if [[ "$METRICS_COUNT" -lt 5 ]]; then
    METRICS_WARNING="WARNING: Only $METRICS_COUNT metrics events (target: > 5)"
    echo "  $METRICS_WARNING"
else
    echo "  Metrics coverage: OK (> 5 metrics events)"
fi

# Step 7: Generate summary report
echo ""
echo "Step 7: Generating summary report..."
TOTAL_EVENTS=$(cat "${JSONL_FILES[@]}" 2>/dev/null | wc -l | tr -d ' ')
if [[ "$PUBLISH_PENDING" == true ]]; then
    OLD_RESULTS_DIR="$TEST_RESULTS_DIR"
    mv "$TEST_RESULTS_DIR" "$FINAL_RUN_ROOT"
    TEST_RESULTS_DIR="$FINAL_RUN_ROOT"
    for index in "${!JSONL_FILES[@]}"; do
        JSONL_FILES[$index]="$FINAL_RUN_ROOT/${JSONL_FILES[$index]#"$OLD_RESULTS_DIR"/}"
    done
    for index in "${!TRACE_FILES[@]}"; do
        TRACE_FILES[$index]="$FINAL_RUN_ROOT/${TRACE_FILES[$index]#"$OLD_RESULTS_DIR"/}"
    done
    echo "  Published verified run bundle after all blocking gates: $FINAL_RUN_ROOT"
fi
MANIFEST_FILE="$TEST_RESULTS_DIR/manifest.json"
COMPLETION_FILE="$TEST_RESULTS_DIR/complete.json"
MANIFEST_SHA256=$(sha256sum "$MANIFEST_FILE" | awk '{print $1}')
COMPLETION_SHA256=$(sha256sum "$COMPLETION_FILE" | awk '{print $1}')
WORKER_HOSTNAME=$(jq -er '.worker_hostname' "$MANIFEST_FILE")
COMMAND_SHA256=$(jq -er '.command_sha256' "$MANIFEST_FILE")
COMMAND_JSON=$(jq -c '.command' "$MANIFEST_FILE")
STARTED_AT_UNIX_MS=$(jq -er '.started_at_unix_ms' "$MANIFEST_FILE")
ENDED_AT_UNIX_MS=$(jq -er '.ended_at_unix_ms' "$MANIFEST_FILE")
GATE_REPORT=$(jq -r '.gates[] | "  \(.name): \(.passed) - \(.detail)"' "$MANIFEST_FILE")

mkdir -p "$TEST_RESULTS_DIR"
cat > "$REPORT_FILE" <<EOF
=== E2E Logging Acceptance Report ===
Generated: $(date -Iseconds)

Run Identity
------------
Run ID: $RUN_ID
Evidence scope: $EVIDENCE_SCOPE
Source SHA: $SOURCE_SHA
Source diff SHA-256: $SOURCE_DIFF_SHA256
Worker ID: $WORKER_ID
Worker hostname: $WORKER_HOSTNAME
Command: $COMMAND_JSON
Command SHA-256: $COMMAND_SHA256
Started Unix ms: $STARTED_AT_UNIX_MS
Ended Unix ms: $ENDED_AT_UNIX_MS
Manifest SHA-256: $MANIFEST_SHA256
Completion receipt SHA-256: $COMPLETION_SHA256

Test Execution
--------------
Exit code: $TEST_EXIT
Quick mode: $QUICK_MODE
Test output: ${TEST_OUTPUT_FILE:-n/a}

File Coverage
-------------
JSONL/log files: $JSONL_COUNT
Total events: $TOTAL_EVENTS
Trace files: ${#TRACE_FILES[@]}
Non-empty trace files: $NONEMPTY_TRACE_COUNT
Trace bytes: $TRACE_BYTES
Trace events: $TRACE_EVENTS

Event Type Coverage
-------------------
Required events: $REQUIRED_EVENTS
All required present: YES
Unique event types: $UNIQUE_EVENT_COUNT

Phase Coverage
--------------
phase_start: $PHASE_START_COUNT
phase_end: $PHASE_END_COUNT
Status: $([ "$PHASE_END_COUNT" -ge 10 ] && echo "PASS (>= 10)" || echo "WARN (< 10)")

Metrics Coverage
----------------
metrics events: $METRICS_COUNT
Status: $([ "$METRICS_COUNT" -ge 5 ] && echo "PASS (>= 5)" || echo "WARN (< 5)")

Manifest Gates
--------------
$GATE_REPORT

Files Validated
---------------
$(printf '%s\n' "${JSONL_FILES[@]}")
$(printf '%s\n' "${TRACE_FILES[@]}")

=== End Report ===
EOF

echo "  Report saved to: $REPORT_FILE"
echo ""
cat "$REPORT_FILE"

# Final result
echo ""
echo "============================================="
if [[ $TEST_EXIT -ne 0 ]]; then
    echo "=== ACCEPTANCE TEST FAILED ==="
    echo "E2E test exit code: $TEST_EXIT"
    exit 1
elif [[ -z "$PHASE_WARNING" ]] && [[ -z "$METRICS_WARNING" ]]; then
    echo "=== ACCEPTANCE TEST PASSED ==="
    exit 0
else
    echo "=== ACCEPTANCE TEST PASSED (with warnings) ==="
    [[ -n "$PHASE_WARNING" ]] && echo "  - $PHASE_WARNING"
    [[ -n "$METRICS_WARNING" ]] && echo "  - $METRICS_WARNING"
    echo ""
    echo "Note: Logging infrastructure is working. Warnings indicate"
    echo "some tests may need additional instrumentation."
    exit 0
fi
