#!/usr/bin/env bash
# Browser E2E lane (bead browser-e2e-lane-bnueb): run the Playwright HTML-export and Pages specs
# against a freshly built cass and write a receipt. GitHub Actions is off, so this script is the
# lane. Run it on a dsr build host, never on ts1 or another shared development machine
# (AGENTS.md "E2E Browser Tests"), and cite the receipt it writes.
#
# Usage: scripts/browser_e2e_lane.sh [--project NAME] [--skip-build] [SPEC ...]
#   --project NAME  Playwright project (default chromium; repeatable)
#   --skip-build    reuse the release binaries already in $CARGO_TARGET_DIR/release
#   SPEC            spec paths relative to tests/ (default: the lane's spec set below)
# Env: CARGO_TARGET_DIR (default <repo>/target), LANE_RECEIPT (default tests/e2e-lane-receipt.json),
#      LANE_REPO (default: the checkout holding this script)
#
# Exit status is Playwright's. The receipt counts skipped tests separately: a spec whose tests
# all skipped did not run, and is not a pass.
set -euo pipefail

repo="${LANE_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
projects=()
build=1
specs=()
while [ $# -gt 0 ]; do
    case "$1" in
        --project) projects+=("$2"); shift 2 ;;
        --skip-build) build=0; shift ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) specs+=("$1"); shift ;;
    esac
done
[ ${#projects[@]} -gt 0 ] || projects=(chromium)
[ ${#specs[@]} -gt 0 ] || specs=(
    e2e/encryption/password-flow.spec.ts
    e2e/export/html-export-e2e.spec.ts
    e2e/export/pages-pipeline-e2e.spec.ts
    e2e/offline/cdn-fallback.spec.ts
)

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
receipt="${LANE_RECEIPT:-$repo/tests/e2e-lane-receipt.json}"
exe=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=".exe" ;; esac
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

if [ "$build" = 1 ]; then
    (cd "$repo" && cargo build --locked --release --bin cass --bin cass-pages-perf-bundle)
fi
cass="$CARGO_TARGET_DIR/release/cass$exe"
[ -x "$cass" ] || { echo "browser lane: no cass binary at $cass" >&2; exit 2; }

cd "$repo/tests"
[ -d node_modules/@playwright/test ] || npm ci --no-audit --no-fund
project_args=()
browsers=()
for project in "${projects[@]}"; do
    project_args+=("--project=$project")
    case "$project" in
        chromium|firefox|webkit) browsers+=("$project") ;;
        iphone-*|mobile-safari) browsers+=(webkit) ;;
        *) browsers+=(chromium) ;;
    esac
done
# The pinned Playwright's browser builds (a no-op when they are already installed).
npx playwright install "${browsers[@]}"

# Fresh exports and Pages bundle from this binary (tests/e2e/setup/global-setup.ts).
set +e
E2E_SKIP_REGENERATE=0 npx playwright test "${project_args[@]}" "${specs[@]}"
status=$?
set -e

browser_versions="$(node --input-type=module -e '
import { chromium, firefox, webkit } from "playwright-core";
const out = {};
for (const [name, type] of Object.entries({ chromium, firefox, webkit })) {
  try { const b = await type.launch(); out[name] = b.version(); await b.close(); }
  catch { out[name] = null; }
}
console.log(JSON.stringify(out));
' 2>/dev/null || echo '{}')"

LANE_STATUS="$status" LANE_HOST="$(hostname)" LANE_OS="$(uname -srm)" \
LANE_COMMIT="$(git -C "$repo" rev-parse HEAD)" \
LANE_DIRTY="$(git -C "$repo" status --porcelain --untracked-files=no | wc -l | tr -d ' ')" \
LANE_CASS_SHA="$(sha256 "$cass")" LANE_CASS_VERSION="$("$cass" --version 2>/dev/null | head -1)" \
LANE_PLAYWRIGHT="$(npx playwright --version 2>/dev/null | head -1)" LANE_BROWSERS="$browser_versions" \
LANE_PROJECTS="${projects[*]}" LANE_RECEIPT_PATH="$receipt" node --input-type=module -e '
import { readFileSync, writeFileSync } from "fs";
const report = JSON.parse(readFileSync("e2e-results.json", "utf8"));
const specs = {};
const walk = (suite) => {
  for (const spec of suite.specs ?? []) {
    const row = (specs[spec.file] ??= { passed: 0, failed: 0, skipped: 0, flaky: 0, failures: [], skips: [] });
    for (const test of spec.tests ?? []) {
      const last = test.results?.[test.results.length - 1];
      if (test.status === "skipped" || last?.status === "skipped") {
        row.skipped++;
        const reason = [...(test.annotations ?? []), ...(last?.annotations ?? [])]
          .find((note) => note.type === "skip")?.description ?? "no reason recorded";
        row.skips.push(`${test.projectName}: ${spec.title} (${reason})`);
      }
      else if (test.status === "flaky") row.flaky++;
      else if (test.status === "expected") row.passed++;
      else { row.failed++; row.failures.push(`${test.projectName}: ${spec.title}`); }
    }
  }
  for (const child of suite.suites ?? []) walk(child);
};
for (const suite of report.suites ?? []) walk(suite);
const env = process.env;
const receipt = {
  lane: "browser-e2e-lane-bnueb",
  generated_at: new Date().toISOString(),
  host: env.LANE_HOST,
  os: env.LANE_OS,
  cass_commit: env.LANE_COMMIT,
  tracked_files_modified: Number(env.LANE_DIRTY),
  cass_sha256: env.LANE_CASS_SHA,
  cass_version: env.LANE_CASS_VERSION,
  playwright: env.LANE_PLAYWRIGHT,
  browsers: JSON.parse(env.LANE_BROWSERS || "{}"),
  projects: env.LANE_PROJECTS.split(" "),
  playwright_exit: Number(env.LANE_STATUS),
  specs,
};
writeFileSync(env.LANE_RECEIPT_PATH, JSON.stringify(receipt, null, 2) + "\n");
for (const [file, r] of Object.entries(specs)) {
  console.log(`LANE spec=${file} passed=${r.passed} failed=${r.failed} skipped=${r.skipped} flaky=${r.flaky}`);
}
console.log(`LANE receipt=${env.LANE_RECEIPT_PATH} playwright_exit=${env.LANE_STATUS} cass_sha256=${env.LANE_CASS_SHA}`);
'
exit "$status"
