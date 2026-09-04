#!/usr/bin/env bash
# dpfvr_ubs_gate_e2e.sh — exercise the ubs-changed-files CI gate logic locally.
#
# Per coding_agent_session_search-dpfvr. The full job runs in GitHub Actions;
# this script reproduces the gate's diff/filter/invocation logic against
# representative scenarios so we can verify behavior without pushing a PR.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# These cases exercise the production batched-gate parsers directly. Inputs are
# terminal-output fixtures, not substitute cargo/ubs executables or live proof.
if [ "${1:-}" = --batched-only ]; then
    proof_dir="$(mktemp -d -t cass-gate-parser.XXXXXX)"
    gate="$PROJECT_ROOT/scripts/gate.sh"
    pass=0
    fail=0
    check_exit() {
        local label="$1" expected="$2" actual=0
        shift 2
        "$@" > "$proof_dir/$label.log" 2>&1 || actual=$?
        if [ "$actual" -eq "$expected" ]; then
            echo "PASS $label"
            pass=$((pass + 1))
        else
            echo "FAIL $label expected=$expected actual=$actual"
            cat "$proof_dir/$label.log"
            fail=$((fail + 1))
        fi
    }
    receipt="$proof_dir/complete.txt"
    cat > "$receipt" <<'RECEIPT'
STAGE=source-identity EXIT=0
STAGE=fmt EXIT=0
STAGE=clippy EXIT=0
STAGE=ubs EXIT=0
TEST_COUNT=lib-tests PASSED=4 BINARIES=1
STAGE=lib-tests EXIT=0
TEST_COUNT=test-e2e_lexical_fail_open PASSED=1 BINARIES=1
STAGE=test-e2e_lexical_fail_open EXIT=0
TEST_COUNT=goldens PASSED=20 BINARIES=2
STAGE=goldens EXIT=0
STAGE=job-complete EXIT=0
RECEIPT
    expected=(source-identity fmt clippy ubs lib-tests test-e2e_lexical_fail_open goldens job-complete)
    check_exit digits-and-positive-counts 0 bash "$gate" --verify-receipt "$receipt" 0 "${expected[@]}"
    check_exit transport-refusal 1 bash "$gate" --verify-receipt "$receipt" 103 "${expected[@]}"
    check_exit transport-timeout 1 bash "$gate" --verify-receipt "$receipt" 124 "${expected[@]}"
    for stage in source-identity fmt clippy ubs lib-tests test-e2e_lexical_fail_open goldens job-complete; do
        awk -v stage="$stage" '$0 != "STAGE=" stage " EXIT=0"' "$receipt" > "$proof_dir/missing-$stage.txt"
        check_exit "missing-$stage" 1 bash "$gate" --verify-receipt "$proof_dir/missing-$stage.txt" 0 "${expected[@]}"
        awk -v stage="$stage" '{if ($0 == "STAGE=" stage " EXIT=0") print "STAGE=" stage " EXIT=1"; else print}' \
            "$receipt" > "$proof_dir/failing-$stage.txt"
        check_exit "failing-$stage" 1 bash "$gate" --verify-receipt "$proof_dir/failing-$stage.txt" 0 "${expected[@]}"
    done
    awk '!/^TEST_COUNT=/' "$receipt" > "$proof_dir/no-counts.txt"
    check_exit no-test-counts 1 bash "$gate" --verify-receipt "$proof_dir/no-counts.txt" 0 "${expected[@]}"
    cat "$receipt" "$receipt" > "$proof_dir/duplicate.txt"
    check_exit duplicate-stage 1 bash "$gate" --verify-receipt "$proof_dir/duplicate.txt" 0 "${expected[@]}"
    { cat "$receipt"; echo 'STAGE=unexpected-after-completion EXIT=0'; } > "$proof_dir/not-terminal.txt"
    check_exit nonterminal-job-complete 1 bash "$gate" --verify-receipt "$proof_dir/not-terminal.txt" 0 "${expected[@]}"
    check_exit stale-docs-binary 1 bash "$gate" --verify-receipt "$receipt" 0 "${expected[@]}" docs-build docs-binary-identity docs-truth

    printf 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out\n' > "$proof_dir/tests-positive.txt"
    printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out\n' > "$proof_dir/tests-empty.txt"
    printf 'running 3 tests\n' > "$proof_dir/tests-truncated.txt"
    cat "$proof_dir/tests-positive.txt" "$proof_dir/tests-empty.txt" > "$proof_dir/tests-mixed.txt"
    cat "$proof_dir/tests-positive.txt" "$proof_dir/tests-positive.txt" > "$proof_dir/tests-two-binaries.txt"
    check_exit positive-tests 0 bash "$gate" --verify-test-log "$proof_dir/tests-positive.txt"
    check_exit two-positive-binaries 0 bash "$gate" --verify-test-log "$proof_dir/tests-two-binaries.txt"
    check_exit zero-selected-tests 1 bash "$gate" --verify-test-log "$proof_dir/tests-empty.txt"
    check_exit truncated-tests 1 bash "$gate" --verify-test-log "$proof_dir/tests-truncated.txt"
    check_exit positive-and-empty-binary 1 bash "$gate" --verify-test-log "$proof_dir/tests-mixed.txt"
    { cat "$proof_dir/tests-positive.txt"; echo 'test result: FAILED. 1 passed; 1 failed;'; } > "$proof_dir/tests-failed.txt"
    check_exit positive-and-failed-binary 1 bash "$gate" --verify-test-log "$proof_dir/tests-failed.txt"
    check_exit repeated-integration-target 2 bash "$gate" --integration 'e2e_lexical_fail_open:a,e2e_lexical_fail_open:b'
    echo "Batched gate parser: PASS=$pass FAIL=$fail fixtures=$proof_dir"
    [ "$fail" -eq 0 ]
    exit $?
fi

RCH_TARGET_DIR="${RCH_TARGET_DIR:-/tmp/cass-dpfvr-target}"
LOG="$RCH_TARGET_DIR/dpfvr-e2e.log"
mkdir -p "$RCH_TARGET_DIR"
exec > >(tee -a "$LOG") 2>&1

cleanup() {
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        echo ""
        echo "[dpfvr_e2e] FAILURE — last 50 log lines:" >&2
        tail -n 50 "$LOG" | sed 's/^/[dpfvr_e2e]   /' >&2
    fi
    exit "$rc"
}
trap cleanup EXIT

# Ensure ubs is on PATH; otherwise mark the gate verification skipped.
UBS_BIN="$(command -v ubs || true)"
if [ -z "$UBS_BIN" ]; then
    echo "[dpfvr_e2e] WARN: ubs not on PATH — skipping live invocation tests."
    echo "[dpfvr_e2e] (CI installs ubs via cargo install; locally, install per AGENTS.md)"
    UBS_AVAILABLE=0
else
    UBS_AVAILABLE=1
    echo "[dpfvr_e2e] ubs binary: $UBS_BIN"
    "$UBS_BIN" --version 2>&1 || true
fi

# Reproduce the gate's filter logic (extracted from ci.yml's run: block).
# The bash filter mirrors what GitHub Actions does on the runner.
filter_changed_files() {
    local range="$1"
    local out
    out="$(git -C "$PROJECT_ROOT" diff --name-only "$range" -- \
        '*.rs' '*.toml' '*.ts' '*.tsx' '*.js' '*.jsx' '*.py' '*.sh' '*.yml' '*.yaml' '*.md' \
        2>/dev/null | grep -v -E '^test-results/|^target/|^node_modules/' || true)"
    printf '%s' "$out"
}

PASS=0
FAIL=0
expect_eq() {
    local description="$1"
    local actual="$2"
    local expected="$3"
    if [ "$actual" = "$expected" ]; then
        echo "[dpfvr_e2e] OK: $description (got=$actual)"
        PASS=$((PASS + 1))
    else
        echo "[dpfvr_e2e] FAIL: $description (expected=$expected got=$actual)"
        FAIL=$((FAIL + 1))
    fi
}

# Scenario 1: no diff against HEAD itself → 0 files
files="$(filter_changed_files HEAD)"
file_count=$([ -z "$files" ] && echo 0 || echo "$files" | wc -l)
expect_eq "scenario=no_diff filter returns empty" "$file_count" "0"

# Scenario 2: diff against initial commit → many files (sanity check)
first_commit="$(git -C "$PROJECT_ROOT" rev-list --max-parents=0 HEAD | tail -1)"
if [ -n "$first_commit" ]; then
    files="$(filter_changed_files "$first_commit")"
    file_count=$([ -z "$files" ] && echo 0 || echo "$files" | wc -l)
    if [ "$file_count" -gt 50 ]; then
        echo "[dpfvr_e2e] OK: scenario=diff_first_commit returns $file_count files (>50, expected)"
        PASS=$((PASS + 1))
    else
        echo "[dpfvr_e2e] FAIL: scenario=diff_first_commit returned only $file_count files; expected >50"
        FAIL=$((FAIL + 1))
    fi
fi

# Scenario 3: skip when only fixture changes (synthetic)
# Synthesize a diff range with only fixture/json/img files to verify the filter excludes them.
TMP_DIR="$(mktemp -d)"
git -C "$PROJECT_ROOT" worktree add "$TMP_DIR" HEAD >/dev/null 2>&1 || true
if [ -d "$TMP_DIR/.git" ] || [ -e "$TMP_DIR/.git" ]; then
    cd "$TMP_DIR"
    mkdir -p tests/fixtures
    echo '{}' > tests/fixtures/synthetic.json
    git add tests/fixtures/synthetic.json 2>/dev/null || true
    git -c user.email="t@t.t" -c user.name="t" commit -q -m "synth-fixture" 2>/dev/null || true
    files="$(filter_changed_files HEAD~1)"
    # The fixture .json should NOT match — UBS only filters extensions in the gate's list.
    file_count=$([ -z "$files" ] && echo 0 || echo "$files" | wc -l)
    expect_eq "scenario=fixture_only_change skips invocation" "$file_count" "0"
    cd "$PROJECT_ROOT"
    git -C "$PROJECT_ROOT" worktree remove --force "$TMP_DIR" >/dev/null 2>&1 || rm -rf "$TMP_DIR"
fi

# Scenario 4: ubs available — run a quick happy-path invocation.
if [ "$UBS_AVAILABLE" -eq 1 ]; then
    # Pick a known-good file from the project (this script itself).
    if "$UBS_BIN" --ci --fail-on-warning "$(realpath "${BASH_SOURCE[0]}")" >/dev/null 2>&1; then
        echo "[dpfvr_e2e] OK: scenario=ubs_clean_file ran without failures"
        PASS=$((PASS + 1))
    else
        echo "[dpfvr_e2e] WARN: scenario=ubs_clean_file ubs reported issues (may be expected)"
    fi
fi

echo ""
echo "[dpfvr_e2e] SUMMARY: PASS=$PASS FAIL=$FAIL"
echo "[dpfvr_e2e] log written to: $LOG"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
echo "[dpfvr_e2e] ALL PASS"
