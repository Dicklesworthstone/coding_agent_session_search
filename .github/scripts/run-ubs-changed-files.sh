#!/usr/bin/env bash
set -euo pipefail

mkdir -p test-results

range="${UBS_DIFF_RANGE:-}"
if [ -z "$range" ]; then
  if [ "${GITHUB_EVENT_NAME:-}" = "pull_request" ] && [ -n "${GITHUB_BASE_REF:-}" ]; then
    range="origin/${GITHUB_BASE_REF}...HEAD"
  elif [ "${GITHUB_EVENT_NAME:-}" = "push" ] &&
       [ "${GITHUB_REF:-}" = "refs/heads/main" ] &&
       [ -n "${GITHUB_EVENT_BEFORE:-}" ]; then
    range="${GITHUB_EVENT_BEFORE}...HEAD"
  else
    range="origin/main...HEAD"
  fi
fi

base_ref="${UBS_BASE_REF:-}"
if [ -z "$base_ref" ] && [[ "$range" == *"..."* ]]; then
  base_ref="${range%%...*}"
fi
if [ -z "$base_ref" ]; then
  base_ref="$(git merge-base origin/main HEAD)"
fi

echo "UBS diff range: $range"
echo "UBS baseline ref: $base_ref"

mapfile -d '' -t files < <(
  git diff --name-only -z "$range" -- \
    '*.rs' '*.toml' '*.ts' '*.tsx' '*.js' '*.jsx' '*.py' '*.sh' '*.yml' '*.yaml' '*.md' |
    grep -z -v -E '^test-results/|^target/|^node_modules/' || true
)

if [ "${#files[@]}" -eq 0 ]; then
  echo "No UBS-relevant files changed; skipping gate."
  exit 0
fi

printf 'UBS changed files (%s):\n' "${#files[@]}"
printf '  %s\n' "${files[@]}" | head -40

zero_json='{"project":"","timestamp":"","scanners":[],"totals":{"critical":0,"warning":0,"info":0,"files":0}}'

scan_ubs_json() {
  local output="$1"
  local stderr_file="$2"
  local scan_dir="$3"
  shift 3

  local output_abs stderr_abs
  output_abs="$(realpath -m "$output")"
  stderr_abs="$(realpath -m "$stderr_file")"

  if [ "$#" -eq 0 ]; then
    printf '%s\n' "$zero_json" > "$output_abs"
    : > "$stderr_abs"
    return 0
  fi

  local rc=0
  (
    cd "$scan_dir"
    ubs --format=json --ci "$@" > "$output_abs" 2> "$stderr_abs"
  ) || rc=$?
  if [ "$rc" -eq 2 ]; then
    echo "UBS environment/tooling failure while scanning $output_abs" >&2
    tail -80 "$stderr_abs" >&2 || true
    return 2
  fi

  if ! jq -e '.totals' "$output_abs" >/dev/null 2>&1; then
    if grep -q 'no recognizable languages' "$stderr_abs"; then
      printf '%s\n' "$zero_json" > "$output_abs"
      return 0
    fi
    echo "UBS did not produce a valid JSON report for $output_abs" >&2
    tail -80 "$stderr_abs" >&2 || true
    return 2
  fi
}

baseline_dir="${RUNNER_TEMP:-/tmp}/ubs-baseline-${GITHUB_RUN_ID:-local}-$$"
mkdir -p "$baseline_dir"
baseline_files=()

for file in "${files[@]}"; do
  if git cat-file -e "${base_ref}:${file}" 2>/dev/null; then
    mkdir -p "$baseline_dir/$(dirname "$file")"
    git show "${base_ref}:${file}" > "$baseline_dir/$file"
    baseline_files+=("$file")
  fi
done

current_json="test-results/ubs-current.json"
baseline_json="test-results/ubs-baseline.json"
current_stderr="test-results/ubs-current.stderr.log"
baseline_stderr="test-results/ubs-baseline.stderr.log"
report_json="test-results/ubs-report.json"

scan_ubs_json "$baseline_json" "$baseline_stderr" "$baseline_dir" "${baseline_files[@]}"
scan_ubs_json "$current_json" "$current_stderr" "$PWD" "${files[@]}"

jq -n \
  --slurpfile current "$current_json" \
  --slurpfile baseline "$baseline_json" \
  --arg range "$range" \
  --arg base_ref "$base_ref" \
  --argjson files_count "${#files[@]}" \
  '
  def totals($doc):
    ($doc[0].totals // {}) as $t |
    {
      critical: (($t.critical // 0) | tonumber),
      warning: (($t.warning // 0) | tonumber),
      info: (($t.info // 0) | tonumber),
      files: (($t.files // 0) | tonumber)
    };
  totals($current) as $cur |
  totals($baseline) as $base |
  {
    gate: "ubs-changed-files-baseline-delta",
    range: $range,
    baseline_ref: $base_ref,
    changed_files: $files_count,
    current: $current[0],
    baseline: $baseline[0],
    comparison: {
      delta: {
        critical: ($cur.critical - $base.critical),
        warning: ($cur.warning - $base.warning),
        info: ($cur.info - $base.info)
      },
      current_totals: $cur,
      baseline_totals: $base
    }
  }' > "$report_json"

critical_delta="$(jq -r '.comparison.delta.critical' "$report_json")"
warning_delta="$(jq -r '.comparison.delta.warning' "$report_json")"

echo "UBS current totals:  $(jq -c '.comparison.current_totals' "$report_json")"
echo "UBS baseline totals: $(jq -c '.comparison.baseline_totals' "$report_json")"
echo "UBS delta:           $(jq -c '.comparison.delta' "$report_json")"

if [ "$critical_delta" -gt 0 ] || [ "$warning_delta" -gt 0 ]; then
  echo "UBS FAIL: changed files introduce new critical or warning findings."
  exit 1
fi

echo "UBS PASS: no new critical or warning findings versus the base-branch baseline."
