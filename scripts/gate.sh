#!/usr/bin/env bash
# gate.sh — the blocking quality gate, batched into ONE rch fleet admission.
#
# Why this exists (reality check 2026-09-01, WS-A.2): every GitHub workflow is
# disabled, so the only gate is agent-run. Fleet admissions are scarce, and an
# agent that spends one on `cargo check`, then another on clippy, then another
# on tests loses most of an hour to refusals — and the temptation to push
# unverified follows. This script runs exactly what CI will run, inside a
# single remote job, and prints one receipt line per stage:
#
#   STAGE=<name> EXIT=<code>
#
# The receipt is what a bead closure or a push cites. Nothing here weakens a
# gate: stages run with the same flags CI uses, and a stage failure never stops
# the later stages (you want the whole picture from one admission).
#
# Usage:
#   scripts/gate.sh                 # fmt, clippy, lib tests, targeted integration, goldens
#   scripts/gate.sh --lib-filter 'quill_bridge::tests health_watermark'   # narrow the lib run
#   scripts/gate.sh --no-lib        # skip the lib suite (e.g. while bet45 wedge is open)
#   scripts/gate.sh --local         # run stages locally (only where rch is not required)
#   GATE_RETRIES=40 GATE_RETRY_SLEEP=90 scripts/gate.sh   # keep retrying fleet refusals
#
# Exit code: 0 when every stage exited 0, 1 otherwise, 103 if the fleet refused
# every attempt (RCH_REQUIRE_REMOTE=1 forbids local fallback).

set -uo pipefail

test_log_counts() {
    awk '
        /^running [0-9]+ tests?$/ {
            if (started != binaries) incomplete++
            started++
        }
        /^test result: ok\. [0-9]+ passed;/ {
            if (started != binaries + 1) incomplete++
            binaries++; passed += $4; if ($4 == 0) empty++
        }
        /^test result: FAILED\./ { empty++ }
        END {
            if (binaries == 0 || empty > 0 || incomplete > 0 || started != binaries) exit 1
            printf "PASSED=%d BINARIES=%d\n", passed, binaries
        }
    ' "$1"
}

verify_receipt() {
    local receipt="$1" remote_rc="$2"
    shift 2
    local failed=0 line name stage last_stage=""
    local -A seen=() counts=()
    if [ "$remote_rc" -ne 0 ]; then
        echo "STAGE=rch EXIT=${remote_rc} (remote job did not complete normally)"
        failed=1
    fi
    while IFS= read -r line; do
        if [[ "$line" =~ ^TEST_COUNT=([a-z0-9_-]+)\ PASSED=([1-9][0-9]*)\ BINARIES=([1-9][0-9]*)$ ]]; then
            counts["${BASH_REMATCH[1]}"]=1
        elif [[ "$line" =~ ^STAGE=([a-z0-9_-]+)\ EXIT=([0-9]+)$ ]]; then
            name="${BASH_REMATCH[1]}"
            [ -n "${seen[$name]:-}" ] && failed=1
            seen["$name"]=1
            last_stage="$name"
            [ "${BASH_REMATCH[2]}" -eq 0 ] || failed=1
            echo "$line"
        fi
    done < "$receipt"
    for stage in "$@"; do
        if [ -z "${seen[$stage]:-}" ]; then
            echo "STAGE=${stage} EXIT=missing (no terminal receipt)"
            failed=1
        fi
        case "$stage" in
            lib-tests|test-*|goldens|goldens-regen)
                if [ -z "${counts[$stage]:-}" ]; then
                    echo "gate: ${stage} has no positive terminal test count" >&2
                    failed=1
                fi ;;
        esac
    done
    if [ "$last_stage" != job-complete ]; then
        echo "gate: missing final job-complete receipt" >&2
        failed=1
    fi
    return "$failed"
}

# Exercise the exact receipt parser without admitting a build or substituting
# tools. The shell regression suite feeds it recorded terminal-output shapes.
if [ "${1:-}" = --verify-receipt ]; then
    shift
    verify_receipt "$@"
    exit $?
fi
if [ "${1:-}" = --verify-test-log ]; then
    test_log_counts "$2"
    exit $?
fi

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_ROOT"

LIB_FILTER=""
RUN_LIB=1
RUN_INTEGRATION=1
RUN_GOLDENS=1
LOCAL=0
REGEN_GOLDENS=0
RUN_DOCS_TRUTH=0
UBS_FILES=()
# Each entry is `<test-binary>` or `<test-binary>:<filter words>`; the filter
# is passed after `--` so one admission can run only the tests a change touches.
INTEGRATION_TESTS=(cli_robot bookmarks_cli)
while [ $# -gt 0 ]; do
    case "$1" in
        --lib-filter) LIB_FILTER="$2"; shift 2 ;;
        --no-lib) RUN_LIB=0; shift ;;
        --local) LOCAL=1; shift ;;
        --ubs-files) IFS=',' read -r -a UBS_FILES <<<"$2"; shift 2 ;;
        --integration) IFS=',' read -r -a INTEGRATION_TESTS <<<"$2"; shift 2 ;;
        # Regenerate goldens (UPDATE_GOLDENS=1) before verifying them. The
        # verify stage still runs, and every resulting diff under tests/golden
        # must be read before it is committed — regeneration is never a way to
        # make a red golden green.
        --regen-goldens) REGEN_GOLDENS=1; shift ;;
        # The fleet closes an SSH session after 30 minutes (rch E104). A full
        # lib suite plus integration plus goldens does not fit in one job on a
        # busy worker, so split: `--lib-only` (fmt, clippy, lib suite) and a
        # second run with `--no-lib` for integration + goldens.
        --lib-only) RUN_INTEGRATION=0; RUN_GOLDENS=0; shift ;;
        --no-goldens) RUN_GOLDENS=0; shift ;;
        # README ↔ code truth (WS-A.9): key bindings, `cass … --flag` usages
        # and env vars, checked with the debug binary the integration stage
        # just built. Opt-in until the README is clean against main.
        --docs-truth) RUN_DOCS_TRUTH=1; shift ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# The fleet's warm target dir. Deliberately NOT the ambient CARGO_TARGET_DIR:
# that variable points at a local build dir in many agent shells, and passing
# it through made a gate run compile clippy from scratch on the worker
# (13 minutes instead of 5). Override with GATE_TARGET_DIR when needed.
TARGET_DIR="${GATE_TARGET_DIR:-/data/tmp/cass-check-target}"
RETRIES="${GATE_RETRIES:-1}"
RETRY_SLEEP="${GATE_RETRY_SLEEP:-90}"
BUILD_JOBS="${GATE_BUILD_JOBS:-2}"
UBS_TIMEOUT="${GATE_UBS_TIMEOUT_SECS:-300}"
if [[ ! "$BUILD_JOBS" =~ ^[1-9][0-9]*$ ]] || [[ ! "$RETRIES" =~ ^[1-9][0-9]*$ ]]; then
    echo "gate: GATE_BUILD_JOBS and GATE_RETRIES must be positive integers" >&2
    exit 2
fi
if [[ ! "$UBS_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
    echo "gate: GATE_UBS_TIMEOUT_SECS must be a positive integer" >&2
    exit 2
fi
SOURCE_HEAD="$(git rev-parse HEAD)" || exit 1
if [ "${#UBS_FILES[@]}" -eq 0 ]; then
    UBS_BASE="${GATE_BASE:-$(git merge-base origin/main "$SOURCE_HEAD")}" || exit 1
    while IFS= read -r -d '' path; do
        [ -f "$path" ] && UBS_FILES+=("$path")
    done < <({
        git diff --name-only -z "$UBS_BASE" -- '*.rs' '*.py' '*.sh' '*.js' '*.ts' '*.tsx' '*.jsx'
        git ls-files --others --exclude-standard -z -- '*.rs' '*.py' '*.sh' '*.js' '*.ts' '*.tsx' '*.jsx'
    } | sort -zu)
fi
UBS_FILES_ARG=""
if [ "${#UBS_FILES[@]}" -gt 0 ]; then
    for path in "${UBS_FILES[@]}"; do
        if [[ "$path" = /* || "$path" = .. || "$path" = ../* || "$path" = */../* ]] || [ ! -f "$path" ]; then
            echo "gate: UBS source must be an existing repository-relative file: ${path}" >&2
            exit 2
        fi
        printf -v quoted_path ' %q' "./${path}"
        UBS_FILES_ARG+="$quoted_path"
    done
fi
source_digest() {
    {
        git diff --binary HEAD
        git ls-files --others --exclude-standard -z | xargs -0 -r sha256sum
    } | sha256sum | cut -d' ' -f1
}
SOURCE_DIFF="$(source_digest)" || exit 1
# RCH 1.0.63 does not combine --job with clean-overlay/content-receipt.
# Verify the transferred inputs ourselves before running any stage.
SOURCE_PATHS="$(git ls-files --cached --others --exclude-standard -z -- \
    Cargo.toml Cargo.lock rust-toolchain.toml build.rs src tests benches scripts \
    .cargo .github README.md | sort -zu | gzip -c | base64 -w0)" || exit 1
SOURCE_CONTENT="$(printf '%s' "$SOURCE_PATHS" | base64 -d | gzip -d | \
    xargs -0 -r sha256sum | sha256sum | cut -d' ' -f1)" || exit 1

# Keep complete terminal output and require a positive test count. Cargo exits
# zero when a misspelled filter selects no tests; that is not validation.
run_tests() {
    local stage="$1"
    shift
    local log rc counts
    local -a statuses
    log="$(mktemp -t cass-gate-tests.XXXXXX)"
    "$@" 2>&1 | tee "$log"
    statuses=("${PIPESTATUS[@]}")
    rc=${statuses[0]}
    [ "${statuses[1]}" -eq 0 ] || rc=1
    if [ "$rc" -eq 0 ]; then
        if counts="$(test_log_counts "$log")"; then
            echo "TEST_COUNT=${stage} ${counts}"
        else
            echo "gate: ${stage} has missing or empty test binaries (log=${log})" >&2
            rc=1
        fi
    fi
    echo "STAGE=${stage} EXIT=${rc}"
}

run_ubs() {
    local expected actual scanner expected_sha tool_dir
    expected="$(tr -d '[:space:]' < .github/workflows/ubs-version.txt)"
    # The runner verifies its language modules against embedded release hashes.
    # Updating the pin also requires reviewing the new runner digest here.
    if [ "$expected" != v5.3.13 ]; then
        echo "gate: no reviewed UBS runner digest for ${expected}" >&2
        return 1
    fi
    expected_sha=47474fd2adee9be2af4796b656a68cb2074c95b9f50b8a7de492873b4528703f
    scanner="$(command -v ubs || true)"
    if [ -z "$scanner" ] || [ "$(sha256sum "$scanner" | cut -d' ' -f1)" != "$expected_sha" ]; then
        # Preserve the worker's installed tool. A fresh private directory also
        # prevents an unrelated tool update from racing this invocation.
        tool_dir="$(mktemp -d -t cass-gate-ubs.XXXXXX)" || return 1
        scanner="$tool_dir/ubs"
        echo "gate: acquiring pinned UBS ${expected} in ${tool_dir}"
        curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
            --max-time 60 --user-agent 'OpenAI File Downloader, XaiImageApiFetch/1.0' \
            "https://raw.githubusercontent.com/Dicklesworthstone/ultimate_bug_scanner/${expected}/ubs" \
            --output "$scanner" || return 1
    fi
    if [ "$(sha256sum "$scanner" | cut -d' ' -f1)" != "$expected_sha" ]; then
        echo "gate: UBS runner bytes differ from the pinned release" >&2
        return 1
    fi
    actual="$(bash "$scanner" --version)" || return 1
    echo "UBS_VERSION=${actual} UBS_PIN=${expected} UBS_SHA256=${expected_sha}"
    if [ "$#" -eq 0 ]; then
        echo "UBS_FILES=0 (no changed scanner-supported source files)"
        return 0
    fi
    printf 'UBS_FILE=%s\n' "$@"
    # The pinned runner emits only aggregate Rust counts in JSON/JSONL mode.
    # Text retains categories and source samples needed to diagnose a red gate.
    bash "$scanner" --no-auto-update --format=text --ci --fail-on-warning "$@"
}

# Every stage records its own exit code and preserves complete test output.
lib_stage=""
if [ "$RUN_LIB" = 1 ]; then
    # A wedged test (the bet45 class: a producer parked forever at ~0 CPU)
    # would otherwise hold the fleet admission until the job's own ceiling.
    # `timeout` turns that into a loud EXIT=124 on this stage instead.
    LIB_TIMEOUT="${GATE_LIB_TIMEOUT_SECS:-2400}"
    if [[ ! "$LIB_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
        echo "gate: GATE_LIB_TIMEOUT_SECS must be a positive integer" >&2
        exit 2
    fi
    read -r -a filter_words <<< "$LIB_FILTER"
    filter_args=""
    [ "${#filter_words[@]}" -eq 0 ] || printf -v filter_args ' %q' "${filter_words[@]}"
    lib_stage="run_tests lib-tests timeout ${LIB_TIMEOUT} cargo test --locked -j ${BUILD_JOBS} --lib -- --test-threads=${BUILD_JOBS} ${filter_args};"
fi
# Every stage the remote script will run, in order. The receipt check below
# requires each one to report: a job cut short by the fleet's SSH ceiling
# leaves later stages missing, and a missing stage is RED, never green.
EXPECTED_STAGES=(source-identity fmt clippy)
[ "$RUN_LIB" = 1 ] && EXPECTED_STAGES+=(lib-tests)
integration_stage=""
if [ "$RUN_INTEGRATION" = 1 ]; then
    declare -A integration_targets=()
    for entry in "${INTEGRATION_TESTS[@]}"; do
        [ -n "$entry" ] || continue
        t="${entry%%:*}"
        if [[ ! "$t" =~ ^[a-zA-Z0-9_-]+$ ]]; then
            echo "gate: invalid integration target ${t}" >&2
            exit 2
        fi
        if [ -n "${integration_targets[$t]:-}" ]; then
            echo "gate: repeated target ${t}; combine its filters in one target:filter1 filter2 entry" >&2
            exit 2
        fi
        integration_targets["$t"]=1
        filter=""
        if [ "$entry" != "$t" ]; then
            filter="${entry#*:}"
        fi
        read -r -a filter_words <<< "$filter"
        filter_args=""
        [ "${#filter_words[@]}" -eq 0 ] || printf -v filter_args ' %q' "${filter_words[@]}"
        integration_stage+="run_tests test-${t} cargo test --locked -j ${BUILD_JOBS} --test ${t} -- --test-threads=${BUILD_JOBS} ${filter_args};"
        EXPECTED_STAGES+=("test-${t}")
    done
fi
golden_regen_stage=""
golden_stage=""
if [ "$RUN_GOLDENS" = 1 ]; then
    if [ "$REGEN_GOLDENS" = 1 ]; then
        golden_regen_stage="UPDATE_GOLDENS=1 run_tests goldens-regen cargo test --locked -j ${BUILD_JOBS} --test golden_robot_json --test golden_robot_docs -- --test-threads=${BUILD_JOBS};"
        EXPECTED_STAGES+=(goldens-regen)
    fi
    golden_stage="run_tests goldens cargo test --locked -j ${BUILD_JOBS} --test golden_robot_json --test golden_robot_docs -- --test-threads=${BUILD_JOBS};"
    EXPECTED_STAGES+=(goldens)
fi

docs_truth_stage=""
if [ "$RUN_DOCS_TRUTH" = 1 ]; then
    # Never inspect a pre-existing target binary when the selected tests did
    # not build it. This invocation must establish its source and ELF first.
    printf -v docs_binary '%q' "${TARGET_DIR}/debug/cass"
    docs_truth_stage="cargo build --locked -j ${BUILD_JOBS} --bin cass; build_rc=\$?; echo STAGE=docs-build EXIT=\$build_rc;
if [ \"\$build_rc\" -eq 0 ]; then
    sha256sum ${docs_binary}; echo STAGE=docs-binary-identity EXIT=\$?;
    CASS_BIN=${docs_binary} scripts/validate_docs.sh --truth; echo STAGE=docs-truth EXIT=\$?;
fi;"
    EXPECTED_STAGES+=(docs-build docs-binary-identity docs-truth)
fi

REMOTE_SCRIPT="$(declare -f test_log_counts run_tests run_ubs)
set -o pipefail
actual_content=\$(printf '%s' '${SOURCE_PATHS}' | base64 -d | gzip -d | xargs -0 -r sha256sum | sha256sum | cut -d' ' -f1)
identity_rc=\$?
if [ \"\$actual_content\" != '${SOURCE_CONTENT}' ]; then identity_rc=1; fi
echo SOURCE_HEAD=${SOURCE_HEAD} SOURCE_CONTENT_SHA256=\$actual_content EXPECTED=${SOURCE_CONTENT}
echo STAGE=source-identity EXIT=\${identity_rc}
if [ \"\$identity_rc\" -ne 0 ]; then exit 1; fi
cargo fmt --check; echo STAGE=fmt EXIT=\$?; \
cargo clippy --locked -j ${BUILD_JOBS} --all-targets -- -D warnings; echo STAGE=clippy EXIT=\$?; \
${lib_stage} ${integration_stage} ${golden_regen_stage} ${golden_stage} ${docs_truth_stage} \
run_ubs ${UBS_FILES_ARG}; echo STAGE=ubs EXIT=\$?; echo STAGE=job-complete EXIT=0"
# Scan after behavioral validation: a slow scanner must not consume the whole
# admission before any product test runs. UBS remains mandatory and blocking.
EXPECTED_STAGES+=(ubs job-complete)

run_once() {
    if [ "$LOCAL" = 1 ]; then
        env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=16777216 UBS_MODULE_TIMEOUT="$UBS_TIMEOUT" bash -c "$REMOTE_SCRIPT"
        return $?
    fi
    RCH_REQUIRE_REMOTE=1 rch exec --job --result-dir tests/golden -- env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=16777216 UBS_MODULE_TIMEOUT="$UBS_TIMEOUT" bash -c "$REMOTE_SCRIPT"
}

# The receipt survives a RED run for post-mortem (GATE_RECEIPT_FILE overrides).
receipt_file="${GATE_RECEIPT_FILE:-$(mktemp -t cass-gate.XXXXXX)}"
if [ -s "$receipt_file" ]; then
    echo "gate: refusing to overwrite existing receipt ${receipt_file}" >&2
    exit 2
fi
printf 'SOURCE_HEAD=%s SOURCE_DIFF_SHA256=%s SOURCE_CONTENT_SHA256=%s\n' \
    "$SOURCE_HEAD" "$SOURCE_DIFF" "$SOURCE_CONTENT" >> "$receipt_file"
attempt=1
rc=103
while [ "$attempt" -le "$RETRIES" ]; do
    echo "gate attempt ${attempt}/${RETRIES} $(date +%T)" >&2
    # Both streams: rch relays remote output on whichever stream it chooses,
    # and a receipt that misses the STAGE lines reads as a truncated job.
    attempt_file="$(mktemp "${receipt_file}.attempt-${attempt}.XXXXXX")"
    run_once 2>&1 | tee "$attempt_file"
    pipeline_status=("${PIPESTATUS[@]}")
    rc=${pipeline_status[0]}
    [ "${pipeline_status[1]}" -eq 0 ] || rc=1
    cat "$attempt_file" >> "$receipt_file"
    if [ "$rc" -ne 103 ]; then
        break
    fi
    attempt=$((attempt + 1))
    [ "$attempt" -le "$RETRIES" ] && sleep "$RETRY_SLEEP"
done

if [ "$rc" -eq 103 ]; then
    echo "gate: fleet refused every attempt (exit 103); nothing was verified" >&2
    echo "gate: receipt kept at ${receipt_file}" >&2
    exit 103
fi

echo "---- gate receipt $(date -u +%Y-%m-%dT%H:%M:%SZ) HEAD=${SOURCE_HEAD} DIFF_SHA256=${SOURCE_DIFF} rch_exit=${rc} receipt=${receipt_file} lines=$(wc -l < "$receipt_file") ----"
failed=0
if [ "$(git rev-parse HEAD)" != "$SOURCE_HEAD" ] || \
   [ "$(source_digest)" != "$SOURCE_DIFF" ]; then
    echo "STAGE=source-stability EXIT=1 (checkout changed during validation)"
    failed=1
fi
verify_receipt "$attempt_file" "$rc" "${EXPECTED_STAGES[@]}" || failed=1
if [ "$failed" -ne 0 ]; then
    echo "gate: RED (receipt kept at ${receipt_file})" >&2
    exit 1
fi
echo "gate: GREEN (receipt kept at ${receipt_file})"
