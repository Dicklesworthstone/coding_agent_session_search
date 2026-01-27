#!/usr/bin/env bash
# E2E JSONL Log Validator
#
# Validates that E2E test log files conform to the expected schema.
# Exit code 0 = valid, non-zero = invalid
#
# Usage: ./scripts/validate-e2e-jsonl.sh [file.jsonl ...]
#        ./scripts/validate-e2e-jsonl.sh test-results/e2e/*.jsonl
#
# Part of T7.1: JSONL log validator + CI gate

set -eo pipefail

# Stats
total_files=0
valid_files=0
invalid_files=0
total_events=0

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
        test_start|test_end)
            if ! echo "$line" | jq -e '.test.name' >/dev/null 2>&1; then
                errors+=("$file:$line_num: $event missing 'test.name' field")
                return 1
            fi
            ;;
        test_end)
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

validate_file() {
    local file="$1"
    local file_valid=true
    local line_num=0
    local has_run_start=false
    local has_test_start=false

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

    # Validate each line
    while IFS= read -r line || [[ -n "$line" ]]; do
        ((line_num++)) || true
        ((total_events++)) || true

        # Skip empty lines
        [[ -z "${line// }" ]] && continue

        # Track event types
        local event
        event=$(echo "$line" | jq -r '.event // empty' 2>/dev/null) || true
        [[ "$event" == "run_start" ]] && has_run_start=true
        [[ "$event" == "test_start" ]] && has_test_start=true

        # Validate the line
        if ! validate_line "$file" "$line_num" "$line"; then
            file_valid=false
        fi
    done < "$file"

    # Structural validation
    if [[ "$has_test_start" == true ]] && [[ "$has_run_start" != true ]]; then
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
        # Default: validate all JSONL files in test-results/e2e
        if [[ -d "test-results/e2e" ]]; then
            shopt -s nullglob
            files=(test-results/e2e/*.jsonl)
            shopt -u nullglob
        fi
    fi

    if [[ ${#files[@]} -eq 0 ]]; then
        echo "No JSONL files found to validate."
        echo "Usage: $0 [file.jsonl ...]"
        exit 0
    fi

    # Validate each file
    for file in "${files[@]}"; do
        ((total_files++)) || true
        if validate_file "$file"; then
            ((valid_files++)) || true
        else
            ((invalid_files++)) || true
        fi
        echo ""
    done

    # Summary
    echo "======================="
    echo "Summary"
    echo "======================="
    echo "Files checked: $total_files"
    echo "Valid: $valid_files"
    echo "Invalid: $invalid_files"
    echo "Total events: $total_events"

    # Print errors
    if [[ ${#errors[@]} -gt 0 ]]; then
        echo ""
        echo "Errors:"
        for err in "${errors[@]}"; do
            echo "  - $err"
        done
    fi

    # Exit code
    if [[ $invalid_files -gt 0 ]]; then
        exit 1
    fi
    exit 0
}

main "$@"
