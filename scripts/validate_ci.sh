#!/bin/bash
# scripts/validate_ci.sh

set -e

# Parse arguments
NO_MOCK_ONLY=false
ARTIFACT_HYGIENE_ONLY=false
NO_MOCK_FAILED=false
E2E_COMPLIANCE_FAILED=false
RCH_BIN=${RCH_BIN:-rch}
RCH_TARGET_DIR=${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_validate_ci}
CASS_ROUTINE_FEATURES="${CASS_ROUTINE_FEATURES:-qr encryption backtrace}"

usage() {
    cat <<'USAGE'
Usage: scripts/validate_ci.sh [--no-mock-only|--artifact-hygiene-only]

Environment:
  RCH_BIN=rch         Remote compilation helper binary for heavy Cargo gates
  RCH_TARGET_DIR=...  Remote Cargo target dir for heavy gates
  CASS_ROUTINE_FEATURES="qr encryption backtrace"
                       Cass features for routine gates; excludes
                       strict-path-dep-validation by default
USAGE
}

ensure_rch() {
    if ! command -v "$RCH_BIN" &> /dev/null; then
        echo "ERROR: rch binary not found; validate_ci Cargo gates must be offloaded" >&2
        return 1
    fi
}

run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --no-mock-only)
            NO_MOCK_ONLY=true
            shift
            ;;
        --artifact-hygiene-only)
            ARTIFACT_HYGIENE_ONLY=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ "$NO_MOCK_ONLY" = true ] && [ "$ARTIFACT_HYGIENE_ONLY" = true ]; then
    echo "--no-mock-only and --artifact-hygiene-only are mutually exclusive" >&2
    usage >&2
    exit 2
fi

echo "=== Validating CI Pipeline ==="

# ============================================================
# Repository Artifact Hygiene Check
# ============================================================
if [ "$NO_MOCK_ONLY" != true ] || [ "$ARTIFACT_HYGIENE_ONLY" = true ]; then
    echo "0. Checking repository artifact hygiene..."

    FORBIDDEN_TRACKED=$(
        git ls-files | while IFS= read -r path; do
            case "$path" in
                .ntm/*|.claude/*|.ruff_cache/*|artifacts/*|ci-artifacts/*|data/*|logs/*|proptest-regressions/*|refactor/*|test-results/*|tests/artifacts/*|tests/e2e/exports/*|tests/e2e/pages_preview/*|tests/test-results/*|tests/tests/*|tmp/*)
                    echo "$path"
                    continue
                    ;;
                *.db-wal|*.db-shm|*.db-journal|*.sqlite-wal|*.sqlite-shm|*.sqlite-journal|*.sqlite3-wal|*.sqlite3-shm|*.sqlite3-journal|*.sqlite3.bak|*.sqlite3.bak-*|*.sqlite3-wal.bak|*.sqlite3.corrupt-*|*.sqlite3-wal.corrupt-*)
                    echo "$path"
                    continue
                    ;;
            esac

            if [[ "$path" != */* ]]; then
                case "$path" in
                    .aider.chat.history.md|a.out|claude-upgrade-progress.json|clippy*.txt|fix_*.py|*.log|*.mcp.json|parse_ubs.py|path_test|perf.data|perf.data.old|scan.txt|storage.sqlite3*|temp_*.rs|test_empty_db.rs|ubs.json|ubs_filtered.txt|ubs-report.*|*.sarif)
                        echo "$path"
                        ;;
                esac
            fi
        done
    )

    if [ -n "$FORBIDDEN_TRACKED" ]; then
        echo "  ERROR: generated or local-only artifacts are tracked:"
        printf '%s\n' "$FORBIDDEN_TRACKED" | sed 's/^/    - /'
        echo ""
        echo "  Move durable evidence to docs/artifacts/, docs/assets/, docs/planning/,"
        echo "  or tests/policies/ as appropriate. Keep generated run output, local"
        echo "  SQLite state, logs, scratch files, and agent harness state ignored."
        exit 1
    fi

    echo "  No forbidden tracked artifacts found - OK"

    if [ "$ARTIFACT_HYGIENE_ONLY" = true ]; then
        exit 0
    fi
fi

# ============================================================
# No-Mock Policy Check
# ============================================================
echo "1. Checking no-mock policy compliance..."

if [ "$SKIP_NO_MOCK_CHECK" = "1" ]; then
    echo "  (Skipping no-mock check: SKIP_NO_MOCK_CHECK=1)"
elif command -v rg &> /dev/null && command -v jq &> /dev/null; then
    ALLOWLIST_FILE="tests/policies/no_mock_allowlist.json"
    VIOLATIONS_FILE=$(mktemp)

    # Search for mock/fake/stub patterns
    # Use explicit patterns to avoid false positives with -i flag
    # - CamelCase: MockFoo, FakeBar, StubBaz (without -i, exact case)
    # - snake_case: mock_, fake_, stub_ (case insensitive)
    # Exclude node_modules (anywhere), target, .git, and fixture files
    rg -n "(Mock[A-Z][a-z]|Fake[A-Z][a-z]|Stub[A-Z][a-z]|mock_|fake_|stub_)" \
        --glob '!**/node_modules/**' \
        --glob '!target/**' \
        --glob '!.git/**' \
        --glob '!tests/fixtures/**' \
        --glob '!test-results/**' \
        --glob '!*.md' \
        --glob '!*.json' \
        src/ tests/ 2>/dev/null > "$VIOLATIONS_FILE" || true

    # Count violations
    VIOLATION_COUNT=$(wc -l < "$VIOLATIONS_FILE" | tr -d ' ')

    if [ "$VIOLATION_COUNT" -gt 0 ]; then
        echo "  Found $VIOLATION_COUNT mock/fake/stub pattern(s)"

        # Check if allowlist exists
        if [ -f "$ALLOWLIST_FILE" ]; then
            ALLOWLIST_ENTRIES=$(jq -r '.entries[] | "\(.path):\(.pattern)"' "$ALLOWLIST_FILE" 2>/dev/null || echo "")
            UNALLOWED_COUNT=0

            while IFS= read -r line; do
                FILE=$(echo "$line" | cut -d: -f1)
                PATTERN=$(echo "$line" | grep -oiE "(Mock[A-Z][a-zA-Z]*|Fake[A-Z][a-zA-Z]*|Stub[A-Z][a-zA-Z]*|mock_[a-z_]+|fake_[a-z_]+|stub_[a-z_]+)" | head -1)

                # Check if this file:pattern combination is allowlisted
                ALLOWED=false
                for entry in $ALLOWLIST_ENTRIES; do
                    ENTRY_PATH=$(echo "$entry" | cut -d: -f1)
                    ENTRY_PATTERN=$(echo "$entry" | cut -d: -f2)

                    if [[ "$FILE" == *"$ENTRY_PATH"* ]] && [[ "$PATTERN" == *"$ENTRY_PATTERN"* || "$ENTRY_PATTERN" == *"$PATTERN"* ]]; then
                        ALLOWED=true
                        break
                    fi
                done

                if [ "$ALLOWED" = false ]; then
                    echo "  VIOLATION: $line"
                    UNALLOWED_COUNT=$((UNALLOWED_COUNT + 1))
                fi
            done < "$VIOLATIONS_FILE"

            if [ "$UNALLOWED_COUNT" -gt 0 ]; then
                echo ""
                echo "  ERROR: $UNALLOWED_COUNT unapproved mock/fake/stub pattern(s) found!"
                echo "  See TESTING.md 'No-Mock Policy' for how to request an exception."
                echo ""
                rm -f "$VIOLATIONS_FILE"
                if [ "$NO_MOCK_ONLY" = true ]; then
                    exit 1
                else
                    # Continue with other checks but mark as failed
                    NO_MOCK_FAILED=true
                fi
            else
                echo "  All patterns are allowlisted - OK"
            fi
        else
            echo "  ERROR: Allowlist file not found at $ALLOWLIST_FILE"
            echo "  Run 'br show bd-28iz' for setup instructions"
            if [ "$NO_MOCK_ONLY" = true ]; then
                rm -f "$VIOLATIONS_FILE"
                exit 1
            fi
            NO_MOCK_FAILED=true
        fi
    else
        echo "  No mock/fake/stub patterns found - OK"
    fi

    rm -f "$VIOLATIONS_FILE"
else
    MISSING_TOOLS=()
    command -v rg &> /dev/null || MISSING_TOOLS+=("rg")
    command -v jq &> /dev/null || MISSING_TOOLS+=("jq")
    echo "  ERROR: cannot run no-mock check; missing required tool(s): ${MISSING_TOOLS[*]}"
    echo "  Install the missing tool(s), or set SKIP_NO_MOCK_CHECK=1 for an explicit local bypass."
    if [ "$NO_MOCK_ONLY" = true ]; then
        exit 1
    fi
    NO_MOCK_FAILED=true
fi

# Exit early if --no-mock-only flag was passed
if [ "$NO_MOCK_ONLY" = true ]; then
    if [ "$NO_MOCK_FAILED" = true ]; then
        exit 1
    fi
    echo "  No-mock check passed"
    exit 0
fi

echo "2. Checking workflow syntax..."
# Requires 'yq' or similar, skipping strict syntax check for now if not present
if command -v yq &> /dev/null; then
    for f in .github/workflows/*.yml; do
        echo "  Validating $f"
        yq . "$f" > /dev/null || { echo "Invalid YAML: $f"; exit 1; }
    done
else
    echo "  (Skipping YAML syntax check: yq not found)"
fi

echo "3. Running local CI simulation..."
ensure_rch
echo "  - Using rch target dir: $RCH_TARGET_DIR"
echo "  - Using cass features: $CASS_ROUTINE_FEATURES"
echo "  - Checking formatting..."
run_cargo fmt --all -- --check

echo "  - Running Clippy..."
run_cargo clippy --all-targets --features "$CASS_ROUTINE_FEATURES" -- -D warnings

echo "  - Running Rust tests..."
run_cargo test --features "$CASS_ROUTINE_FEATURES"

echo "  - Running Crypto Vector tests..."
run_cargo test --test crypto_vectors

echo "  - Running cargo audit (if installed)..."
if run_cargo audit --version >/dev/null 2>&1; then
    run_cargo audit
else
    echo "    (Skipping cargo audit: cargo-audit not available through rch)"
fi

if [ -f "web/package.json" ]; then
    echo "  - Running web tests (if npm is available)..."
    if command -v npm >/dev/null 2>&1; then
        (cd web && npm ci && npm test)
    else
        echo "    (Skipping web tests: npm not found)"
    fi
fi

# ============================================================
# E2E Logging Compliance Check
# ============================================================
echo "4. Checking E2E logging compliance..."

E2E_ERRORS=0
E2E_WARNINGS=0

# Check that all e2e_*.rs files import e2e_log module
for f in tests/e2e_*.rs; do
    if [ ! -f "$f" ]; then
        continue
    fi
    name=$(basename "$f")

    # Check for e2e_log import or PhaseTracker usage
    if ! grep -q "use.*e2e_log\|mod.*e2e_log\|PhaseTracker\|E2eLogger" "$f"; then
        echo "  WARNING: $name not using E2E logging infrastructure"
        ((E2E_WARNINGS++)) || true
    fi
done

# Check shell scripts source e2e_log.sh
for f in scripts/e2e/*.sh; do
    if [ -f "$f" ] && [ -s "$f" ]; then
        name=$(basename "$f")
        if ! grep -q "e2e_log.sh" "$f"; then
            echo "  WARNING: $name not sourcing e2e_log.sh"
            ((E2E_WARNINGS++)) || true
        fi
    fi
done

if [ $E2E_ERRORS -gt 0 ]; then
    echo "  FAILED: $E2E_ERRORS E2E logging compliance error(s)"
    E2E_COMPLIANCE_FAILED=true
else
    if [ $E2E_WARNINGS -gt 0 ]; then
        echo "  OK with $E2E_WARNINGS warning(s) (non-blocking)"
    else
        echo "  OK: E2E logging compliance checks passed"
    fi
fi

# Final check for deferred no-mock failures
if [ "$NO_MOCK_FAILED" = true ]; then
    echo ""
    echo "=== CI Validation FAILED ==="
    echo "No-mock policy violations found. See output above."
    exit 1
fi

if [ "$E2E_COMPLIANCE_FAILED" = true ]; then
    echo ""
    echo "=== CI Validation FAILED ==="
    echo "E2E logging compliance violations found. See output above."
    exit 1
fi

echo "=== CI Validation Complete ==="
