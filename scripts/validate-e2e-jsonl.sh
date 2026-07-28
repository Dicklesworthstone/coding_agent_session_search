#!/usr/bin/env bash
# E2E JSONL Log Validator
#
# Validates that E2E test log files conform to the expected schema.
# Exit code 0 = valid, non-zero = invalid
#
# Usage: ./scripts/validate-e2e-jsonl.sh [file.jsonl ...]
#        ./scripts/validate-e2e-jsonl.sh test-results/e2e/*.jsonl test-results/e2e/**/{cass.log,trace.jsonl}
#
# Part of T7.1: JSONL log validator + CI gate

set -eo pipefail

# Stats
total_files=0
valid_files=0
invalid_files=0
total_events=0
aggregate_failures=0
semantic_trace_bytes=0
semantic_trace_files=0

# Error collection
declare -a errors=()

validate_line() {
    local file="$1"
    local line_num="$2"
    local line="$3"

    # Skip empty lines
    [[ -z "${line// }" ]] && return 0

    # Parse event type
    local event
    event=$(echo "$line" | jq -r '.event // empty' 2>/dev/null) || {
        errors+=("$file:$line_num: Invalid JSON")
        return 1
    }

    if [[ -z "$event" ]]; then
        errors+=("$file:$line_num: Missing 'event' field")
        return 1
    fi

    # Validate common required fields
    for field in ts run_id runner; do
        if ! echo "$line" | jq -e ".$field" >/dev/null 2>&1; then
            errors+=("$file:$line_num: Event '$event' missing required field '$field'")
            return 1
        fi
    done

    # Validate event-specific fields
    case "$event" in
        run_start)
            if ! echo "$line" | jq -e '.env' >/dev/null 2>&1; then
                errors+=("$file:$line_num: run_start missing 'env' field")
                return 1
            fi
            ;;
        test_start)
            if ! echo "$line" | jq -e '.test.name' >/dev/null 2>&1; then
                errors+=("$file:$line_num: $event missing 'test.name' field")
                return 1
            fi
            ;;
        test_end)
            if ! echo "$line" | jq -e '.test.name' >/dev/null 2>&1; then
                errors+=("$file:$line_num: $event missing 'test.name' field")
                return 1
            fi
            if ! echo "$line" | jq -e '.result.status' >/dev/null 2>&1; then
                errors+=("$file:$line_num: test_end missing 'result.status' field")
                return 1
            fi
            ;;
        run_end)
            if ! echo "$line" | jq -e '.summary' >/dev/null 2>&1; then
                errors+=("$file:$line_num: run_end missing 'summary' field")
                return 1
            fi
            ;;
        phase_start|phase_end)
            if ! echo "$line" | jq -e '.phase.name' >/dev/null 2>&1; then
                errors+=("$file:$line_num: $event missing 'phase.name' field")
                return 1
            fi
            ;;
        metrics)
            if ! echo "$line" | jq -e '.metrics' >/dev/null 2>&1; then
                errors+=("$file:$line_num: metrics missing 'metrics' field")
                return 1
            fi
            ;;
    esac

    return 0
}

validate_trace_line() {
    local file="$1"
    local line_num="$2"
    local line="$3"

    [[ -z "${line// }" ]] && return 0

    if ! echo "$line" | jq -e '
        type == "object"
        and .schema_version == "cass-trace-v1"
        and ((.timestamp | type) == "string" and (.timestamp | length) > 0)
        and ((.level | type) == "string" and (.level | length) > 0)
        and ((.target | type) == "string" and (.target | length) > 0)
        and has("trace_id")
        and has("test_id")
        and (.fields | type == "object")
        and (
            ((.fields.event | type) == "string" and (.fields.event | length) > 0)
            or ((.fields.message | type) == "string" and (.fields.message | length) > 0)
        )
    ' >/dev/null 2>&1; then
        errors+=("$file:$line_num: Invalid cass-trace-v1 JSONL envelope")
        return 1
    fi

    if [[ "$file" == *"test-results/e2e/"* ]] && ! echo "$line" | jq -e '
        ((.trace_id | type) == "string" and (.trace_id | length) > 0)
        and ((.test_id | type) == "string" and (.test_id | length) > 0)
    ' >/dev/null 2>&1; then
        errors+=("$file:$line_num: E2E trace correlation IDs must be non-empty strings")
        return 1
    fi

    local receipt
    receipt=$(echo "$line" | jq -r '.fields.event // empty')
    case "$receipt" in
        trace_truncated)
            if ! echo "$line" | jq -e '
                .fields.artifact_complete == false
                and ((.fields.suppressed_events | type) == "number"
                    and .fields.suppressed_events > 0)
                and ((.fields.suppressed_bytes | type) == "number")
                and ((.fields.reason | type) == "string")
                and ((.fields.suppression_reasons | type) == "object")
                and ((.fields.failure_tail_events | type) == "number")
                and ((.fields.failure_tail_bytes | type) == "number")
                and ((.fields.failure_tail_dropped_events | type) == "number")
                and (
                    .fields.suppression_reasons.byte_budget
                    + .fields.suppression_reasons.event_budget
                    + .fields.suppression_reasons.oversize_event
                    == .fields.suppressed_events
                )
            ' >/dev/null 2>&1; then
                errors+=("$file:$line_num: Invalid trace_truncated receipt")
                return 1
            fi
            ;;
        trace_filter_summary)
            if ! echo "$line" | jq -e '
                .fields.artifact_complete == true
                and ((.fields.filtered_events | type) == "number"
                    and .fields.filtered_events > 0)
            ' >/dev/null 2>&1; then
                errors+=("$file:$line_num: Invalid trace_filter_summary receipt")
                return 1
            fi
            ;;
        command_summary)
            if ! echo "$line" | jq -e '
                ((.fields.duration_ms | type) == "number")
                and ((.fields.exit_code | type) == "number")
            ' >/dev/null 2>&1; then
                errors+=("$file:$line_num: command_summary missing duration or outcome")
                return 1
            fi
            ;;
    esac

    return 0
}

validate_file() {
    local file="$1"
    local file_valid=true
    local line_num=0
    local has_run_start=false
    local has_test_start=false
    local has_command_summary=false
    local is_trace_file=false
    [[ "$(basename "$file")" == "trace.jsonl" ]] && is_trace_file=true

    echo "Validating: $file"

    # Check file exists and is readable
    if [[ ! -f "$file" ]]; then
        errors+=("$file: File not found")
        return 1
    fi

    if [[ ! -s "$file" ]]; then
        echo "  Warning: Empty file"
        return 0
    fi

    if [[ "$is_trace_file" == true ]] && [[ "$file" == *"test-results/e2e/"* ]]; then
        local trace_bytes
        trace_bytes=$(wc -c < "$file")
        if (( trace_bytes > 524288 )); then
            errors+=("$file: $trace_bytes bytes exceeds the 524288-byte E2E trace budget")
            file_valid=false
        fi
        if [[ "$file" == *"test-results/e2e/e2e_semantic_search/"* ]]; then
            semantic_trace_bytes=$((semantic_trace_bytes + trace_bytes))
            semantic_trace_files=$((semantic_trace_files + 1))
        fi
        if grep -Eq '/home/|/data/projects/' "$file"; then
            errors+=("$file: trace contains an unredacted private path")
            file_valid=false
        fi
    fi

    # Validate each line. The helper only uses the file path for diagnostics.
    # shellcheck disable=SC2094
    while IFS= read -r line || [[ -n "$line" ]]; do
        ((line_num += 1))
        ((total_events += 1))

        # Skip empty lines
        [[ -z "${line// }" ]] && continue

        # Track event types
        local event
        if [[ "$is_trace_file" == true ]]; then
            event=$(echo "$line" | jq -r '.fields.event // empty' 2>/dev/null) || true
            [[ "$event" == "command_summary" ]] && has_command_summary=true
        else
            event=$(echo "$line" | jq -r '.event // empty' 2>/dev/null) || true
            [[ "$event" == "run_start" ]] && has_run_start=true
            [[ "$event" == "test_start" ]] && has_test_start=true
        fi

        # Validate the line
        if [[ "$is_trace_file" == true ]]; then
            if ! validate_trace_line "$file" "$line_num" "$line"; then
                file_valid=false
            fi
        elif ! validate_line "$file" "$line_num" "$line"; then
            file_valid=false
        fi
    done < "$file"

    if [[ "$is_trace_file" == true ]]; then
        if [[ "$has_command_summary" != true ]]; then
            errors+=("$file: trace has no command_summary outcome record")
            file_valid=false
        fi
        if [[ "$file_valid" == true ]]; then
            echo "  Valid ($line_num trace events)"
            return 0
        fi
        echo "  Invalid"
        return 1
    fi

    # Structural validation
    # Per-test log files (in test subdirectories) don't have run_start - only test_start/test_end
    # Main run logs (at the root e2e level) should have run_start
    local is_per_test_log=false
    if [[ "$file" == *"/cass.log" ]]; then
        local parent_name
        parent_name=$(basename "$(dirname "$file")")
        # If parent directory is not "e2e", it's a per-test log in a subdirectory
        if [[ "$parent_name" != "e2e" ]]; then
            is_per_test_log=true
        fi
    fi

    if [[ "$has_test_start" == true ]] && [[ "$has_run_start" != true ]] && [[ "$is_per_test_log" == false ]]; then
        errors+=("$file: Has test events but no run_start")
        file_valid=false
    fi

    # Count test starts and ends
    local test_starts test_ends
    test_starts=$(jq -s '[.[] | select(.event == "test_start")] | length' "$file" 2>/dev/null || echo 0)
    test_ends=$(jq -s '[.[] | select(.event == "test_end")] | length' "$file" 2>/dev/null || echo 0)

    if [[ "$test_starts" != "$test_ends" ]]; then
        errors+=("$file: Mismatched test_start ($test_starts) and test_end ($test_ends)")
        file_valid=false
    fi

    if [[ "$file_valid" == true ]]; then
        echo "  Valid ($line_num events)"
        return 0
    else
        echo "  Invalid"
        return 1
    fi
}

main() {
    echo "E2E JSONL Log Validator"
    echo "======================="
    echo ""

    # Check for jq
    if ! command -v jq &> /dev/null; then
        echo "Error: jq is required but not installed"
        exit 1
    fi

    # Get files to validate
    local files=("$@")
    if [[ ${#files[@]} -eq 0 ]]; then
        # Default: validate E2E event logs plus separately-versioned per-test traces.
        if [[ -d "test-results/e2e" ]]; then
            while IFS= read -r -d '' file; do
                files+=("$file")
            done < <(
                find test-results/e2e \
                    -type f \( -name "*.jsonl" -o -name "cass.log" \) \
                    ! -name "combined.jsonl" \
                    ! -path "test-results/e2e/.previous/*" \
                    -print0 | sort -z
            )
        fi
    fi

    if [[ ${#files[@]} -eq 0 ]]; then
        echo "No JSONL files found to validate."
        echo "Usage: $0 [file.jsonl ...]"
        exit 0
    fi

    # Validate each file
    for file in "${files[@]}"; do
        ((total_files += 1))
        if validate_file "$file"; then
            ((valid_files += 1))
        else
            ((invalid_files += 1))
        fi
        echo ""
    done

    if (( semantic_trace_bytes > 10485760 )); then
        errors+=("semantic trace aggregate is $semantic_trace_bytes bytes, exceeding the 10485760-byte campaign gate")
        aggregate_failures=$((aggregate_failures + 1))
    fi

    # Summary
    echo "======================="
    echo "Summary"
    echo "======================="
    echo "Files checked: $total_files"
    echo "Valid: $valid_files"
    echo "Invalid: $invalid_files"
    echo "Total events: $total_events"
    echo "Semantic trace files: $semantic_trace_files"
    echo "Semantic trace bytes: $semantic_trace_bytes"
    echo "Aggregate gate failures: $aggregate_failures"

    # Print errors
    if [[ ${#errors[@]} -gt 0 ]]; then
        echo ""
        echo "Errors:"
        for err in "${errors[@]}"; do
            echo "  - $err"
        done
    fi

    # Exit code
    if [[ $invalid_files -gt 0 || $aggregate_failures -gt 0 ]]; then
        exit 1
    fi
    exit 0
}

main "$@"
