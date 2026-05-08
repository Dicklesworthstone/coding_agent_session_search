#!/usr/bin/env bash
# scripts/perf/run_pages_perf.sh
# Performance harness for cass Pages bundles.
#
# Generates synthetic bundles (small/medium/large/xlarge) and runs browser perf checks.
# Artifacts land under target/perf/<preset>/.
#
# Usage:
#   ./scripts/perf/run_pages_perf.sh
#   ./scripts/perf/run_pages_perf.sh --preset large --lighthouse
#   RCH_TARGET_DIR=/tmp/rch_target_cass_pages_perf ./scripts/perf/run_pages_perf.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_pages_perf}"

PRESET_FILTER=""
RUN_LIGHTHOUSE=0
FAIL_FAST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --preset)
      PRESET_FILTER="$2"
      shift 2
      ;;
    --lighthouse)
      RUN_LIGHTHOUSE=1
      shift
      ;;
    --fail-fast)
      FAIL_FAST=1
      shift
      ;;
    --help|-h)
      cat <<EOF
Usage: $0 [--preset <name>] [--lighthouse] [--fail-fast]

Environment:
  RCH_BIN         rch executable (default: rch)
  RCH_TARGET_DIR  cargo target dir for offloaded helper build (default: \${TMPDIR:-/tmp}/rch_target_cass_pages_perf)
EOF
      exit 0
      ;;
    *)
      shift
      ;;
  esac
 done

ensure_rch() {
  if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
    log "ERROR" "rch binary not found; pages perf helper build must be offloaded"
    exit 1
  fi
}

run_cargo() {
  "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

LOG_ROOT="${PROJECT_ROOT}/target/perf"
RUN_ID="$(date +"%Y%m%d_%H%M%S")_${RANDOM}"
RUN_DIR="${LOG_ROOT}/run_${RUN_ID}"
LOG_FILE="${RUN_DIR}/run.log"
JSON_LOG_FILE="${RUN_DIR}/run.jsonl"
SUMMARY_JSON="${RUN_DIR}/summary.json"

mkdir -p "${RUN_DIR}"

log() {
  local level=$1
  shift
  local msg="$*"
  local ts
  ts=$(date +"%Y-%m-%d %H:%M:%S.%3N" 2>/dev/null || date +"%Y-%m-%d %H:%M:%S")
  echo "[${ts}] [${level}] ${msg}" | tee -a "${LOG_FILE}"
  printf '{"ts":"%s","level":"%s","msg":"%s"}\n' "$ts" "$level" "${msg//"/\\"}" >> "${JSON_LOG_FILE}"
}

run_step() {
  local name=$1
  shift
  local start_ts
  local end_ts
  local exit_code

  start_ts=$(date +%s%3N)
  log "PHASE" "${name}"
  log "INFO" "Command: $*"

  set +e
  "$@" >> "${LOG_FILE}" 2>&1
  exit_code=$?
  set -e

  end_ts=$(date +%s%3N)
  local duration_ms=$((end_ts - start_ts))
  printf '{"ts":"%s","event":"step_end","step":"%s","exit_code":%d,"duration_ms":%d}\n' \
    "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" "$name" "$exit_code" "$duration_ms" >> "${JSON_LOG_FILE}"

  if [[ $exit_code -ne 0 ]]; then
    log "ERROR" "${name} failed (${exit_code})"
    if [[ $FAIL_FAST -eq 1 ]]; then
      exit $exit_code
    fi
  else
    log "INFO" "${name} completed in ${duration_ms}ms"
  fi
}

log "INFO" "Run dir: ${RUN_DIR}"
log "INFO" "RCH binary: ${RCH_BIN}"
log "INFO" "RCH target dir: ${RCH_TARGET_DIR}"

cd "${PROJECT_ROOT}"
ensure_rch

BUNDLE_BIN="${RCH_TARGET_DIR}/debug/cass-pages-perf-bundle"
run_step "build_bundle_helper" run_cargo build --quiet --bin cass-pages-perf-bundle
if [[ ! -x "$BUNDLE_BIN" ]]; then
  log "ERROR" "cass-pages-perf-bundle not found at ${BUNDLE_BIN} after rch build"
  exit 1
fi

if [[ ! -d tests/performance/node_modules ]]; then
  log "INFO" "Installing performance test dependencies"
  run_step "npm_install" bash -c "cd tests/performance && npm install"
fi

PRESETS=(small medium large xlarge)
if [[ -n "${PRESET_FILTER}" ]]; then
  PRESETS=("${PRESET_FILTER}")
fi

RESULTS=()
FAILED=()

for preset in "${PRESETS[@]}"; do
  preset_dir="${LOG_ROOT}/${preset}"
  bundle_dir="${preset_dir}/bundle/site"
  perf_json="${preset_dir}/perf.json"

  mkdir -p "${preset_dir}"

  run_step "bundle_${preset}" "$BUNDLE_BIN" \
    --output "${preset_dir}" --preset "${preset}" --json

  perf_cmd=(node tests/performance/run_perf.js --bundle "${bundle_dir}" --out "${perf_json}")
  if [[ $RUN_LIGHTHOUSE -eq 1 ]]; then
    perf_cmd+=(--lighthouse)
  fi

  run_step "perf_${preset}" "${perf_cmd[@]}"

  if [[ ! -f "${perf_json}" ]]; then
    FAILED+=("${preset}")
  else
    RESULTS+=("${preset}")
  fi
done

if [[ ${#RESULTS[@]} -eq 0 ]]; then
  OK_JSON="[]"
else
  OK_JSON=$(printf '[\"%s\"]' "${RESULTS[*]}" | sed 's/ /\",\"/g')
fi

if [[ ${#FAILED[@]} -eq 0 ]]; then
  FAIL_JSON="[]"
else
  FAIL_JSON=$(printf '[\"%s\"]' "${FAILED[*]}" | sed 's/ /\",\"/g')
fi

cat <<EOF > "${SUMMARY_JSON}"
{
  "run_id": "${RUN_ID}",
  "presets": ${OK_JSON},
  "failed": ${FAIL_JSON},
  "log": "${LOG_FILE}",
  "json_log": "${JSON_LOG_FILE}"
}
EOF

log "INFO" "Summary: ${SUMMARY_JSON}"

if [[ ${#FAILED[@]} -ne 0 ]]; then
  log "ERROR" "Failed presets: ${FAILED[*]}"
  exit 1
fi

log "INFO" "Performance run completed"
