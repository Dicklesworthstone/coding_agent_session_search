# Testing Guide

> Guidelines for testing in the cass (Coding Agent Session Search) codebase.

---

## No-Mock Policy

### Philosophy

This project adheres to a **strict no-mock policy** for testing. Instead of mocking external dependencies, we use:

1. **Real implementations** with test configurations
2. **Fixture data** from actual sessions and real scenarios
3. **Integration test harnesses** that exercise real code paths
4. **E2E tests** that validate complete workflows

### Why No Mocks?

Mocks are problematic because they:

- **Hide bugs**: Mocks don't catch when real implementations change behavior
- **Create maintenance burden**: Mock implementations drift from reality
- **Reduce confidence**: Passing tests don't prove the real system works
- **Encourage poor design**: Mocks make it easy to test tightly-coupled code

### What We Use Instead

| Instead of... | Use... |
|---------------|--------|
| Mock connectors | Real session fixtures in `tests/fixtures/connectors/` |
| Mock databases | Real SQLite with test data |
| Mock Tantivy | Real index with small fixture corpus |
| Mock embedders | Hash embedder (fast, deterministic) |
| Mock daemon | Channel-based test harness |
| Mock filesystem | Tempdir with real fixture files |

### Allowlist: True Boundaries

Some scenarios require deterministic fixture constructors. These are explicitly
allowlisted (see `test-results/no_mock_allowlist.json`):

1. **Fixture constructors** (`#[cfg(test)]` only):
   - `mock_system_info` in `src/sources/install.rs` - deterministic SystemInfo for pure logic tests
   - `mock_resources` in `src/sources/install.rs` - deterministic ResourceInfo for pure logic tests

### CI Enforcement

The CI pipeline enforces the no-mock policy:

```bash
# Run the no-mock check
./scripts/validate_ci.sh --no-mock-only

# Skip locally (for development iteration)
SKIP_NO_MOCK_CHECK=1 ./scripts/validate_ci.sh
```

The check:
1. Searches for `Mock*`, `Fake*`, `Stub*`, `mock_`, `fake_`, `stub_` patterns
2. Compares against the allowlist in `test-results/no_mock_allowlist.json`
3. Fails if unapproved patterns are found

### Requesting an Allowlist Exception

To request a new allowlist entry:

1. Create a bead explaining why a real implementation is impossible
2. Add an entry to `test-results/no_mock_allowlist.json`:
   ```json
   {
     "path": "src/your/file.rs",
     "pattern": "MockThing",
     "rationale": "Why real implementation is impossible",
     "owner": "your-team",
     "review_date": "YYYY-MM-DD (max 6 months)",
     "downstream_task": "bd-xxxx (to remove this entry)",
     "cfg_test_only": true
   }
   ```
3. Get approval via code review
4. Entries expire after 6 months and require re-justification

---

## Test Structure

### Unit Tests (`#[cfg(test)]` modules)

In-file unit tests for isolated function/trait behavior:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message() {
        // Test with real JSONL content, not mocked data
        let content = include_str!("../tests/fixtures/messages/sample.jsonl");
        let result = parse_message(content);
        assert!(result.is_ok());
    }
}
```

### Integration Tests (`tests/`)

Tests that exercise multiple components together:

- `tests/connector_*.rs` - Connector parsing with fixture files
- `tests/search_*.rs` - Search pipeline with real indexes
- `tests/semantic_*.rs` - Embedding with hash embedder
- `tests/daemon_client_integration.rs` - Daemon client with channel harness

### E2E Tests

**Rust E2E** (`tests/e2e_*.rs`):
- Full CLI invocation tests
- Real fixtures, real binaries, real outputs

**Browser E2E** (`tests/e2e/`):
- Playwright tests for HTML exports
- Run on CI only (see AGENTS.md "E2E Browser Tests")

**Shell E2E** (`scripts/e2e/*.sh`):
- `connector_stress.sh` - Stress-test connector parsing with malformed/edge fixtures
- `query_parser_e2e.sh` - Exercise query parsing end-to-end via CLI search flows
- `security_paths_e2e.sh` - Validate export path security and traversal hardening
- `full_coverage_validation.sh` - Master runner (executes the scripts above, runs key Rust E2E suites, validates JSONL, and produces coverage + summary)
- All shell scripts must source `scripts/lib/e2e_log.sh` and emit JSONL to `test-results/e2e/`

#### Scenario Coverage (T4.*)

The following scenario-focused E2E suites are complete and tracked:

- Error recovery: `tests/e2e_error_recovery.rs`
- Large datasets: `tests/e2e_large_dataset.rs`
- Mobile devices: `tests/e2e/mobile/*.spec.ts`
- Offline mode: `tests/e2e/offline/*.spec.ts`
- Accessibility: `tests/e2e/accessibility/*.spec.ts`

---

## Fixtures

### Location

All fixture data lives under `tests/fixtures/`:

```
tests/fixtures/
├── connectors/           # Real session files per agent
│   ├── claude/
│   ├── codex/
│   ├── cursor/
│   └── ...
├── html_export/          # Real exported sessions
│   └── real_sessions/
├── messages/             # Sample JSONL messages
├── models/               # Small valid ONNX models (if needed)
└── sources/              # Multi-machine sync fixtures
```

### Creating Fixtures

1. Use real data from actual agent sessions
2. Anonymize sensitive content (usernames, paths, secrets)
3. Keep fixtures small but representative
4. Document the fixture's purpose in a README

### Fixture Helpers Module

Use `tests/fixture_helpers.rs` for setting up connector tests:

```rust
use crate::fixture_helpers::{setup_connector_test, create_project_dir, write_session_file};

#[test]
fn test_my_connector() {
    // Creates temp dir with "fixture-{agent}" naming
    let (dir, data_dir) = setup_connector_test("claude");

    // Create project structure
    let project_dir = create_project_dir(&data_dir, "my-project");
    write_session_file(&project_dir, "session.jsonl", &content);

    // ... run connector tests ...
}
```

**Important**: Use `fixture-{agent}` naming (not `mock-{agent}`) for temp directories.

### Fixture Provenance (MANIFEST.json)

All connector fixtures are tracked in `tests/fixtures/connectors/MANIFEST.json`:

```json
{
  "fixtures": {
    "claude": {
      "source": "tests/fixtures/claude_code_real",
      "capture_date": "2025-11-25",
      "redaction_policy": "usernames_anonymized",
      "files": [
        {
          "path": "projects/-test-project/agent-test123.jsonl",
          "sha256": "89dd0a299dd4e761d185a65b652d6a29982cbc71aa9e07cfa3aa07475696c202"
        }
      ]
    }
  }
}
```

When adding new fixtures:
1. Add an entry to the MANIFEST.json
2. Compute SHA256 hash: `sha256sum <file>`
3. Document the capture date and redaction policy

### Loading Fixtures in Tests

```rust
// Good: Load real fixture
let fixture = include_str!("fixtures/connectors/claude/session.jsonl");

// Bad: Create mock data inline
let mock_session = r#"{"fake": "data"}"#;  // NO!
```

### Model Fixtures for Semantic Search

For tests that require embedding models (semantic search, reranking), use the real
model fixtures in `tests/fixtures/models/`:

```
tests/fixtures/models/
├── xenova-paraphrase-minilm-l3-v2-int8/   # Embedding model (~17 MB)
│   ├── model.onnx
│   ├── tokenizer.json
│   ├── config.json
│   └── checksums.sha256
├── xenova-ms-marco-minilm-l6-v2-int8/     # Reranker model (~22 MB)
│   ├── model.onnx
│   ├── tokenizer.json
│   └── checksums.sha256
└── README.md
```

**Usage in tests:**

```rust
use crate::fixture_helpers::{embedder_fixture_dir, reranker_fixture_dir};

#[test]
fn test_semantic_search() {
    // Get fixture directories with real ONNX models
    let embedder_dir = embedder_fixture_dir();
    let reranker_dir = reranker_fixture_dir();

    // Verify checksums before loading
    verify_model_fixture_checksums(&embedder_dir).expect("valid checksums");

    // Now load and use the real model
    // ...
}
```

**Model sources:**
- Embedding: `Xenova/paraphrase-MiniLM-L3-v2` (Apache-2.0)
- Reranker: `Xenova/ms-marco-MiniLM-L-6-v2` (Apache-2.0)

### SSH Test Fixtures

SSH-related tests use Docker containers with a real SSH server:

**Docker infrastructure:**
```
tests/docker/
├── Dockerfile.sshd    # SSH server image
└── entrypoint.sh      # Container startup script
```

**Probe fixtures:** `tests/fixtures/sources/probe/`
- `indexed_host.json` - Host with cass indexed (847 sessions)
- `not_indexed_host.json` - Host with cass but not indexed
- `no_cass_host.json` - Host without cass installed
- `unreachable_host.json` - Connection failure scenario

**Usage:**

```rust
use crate::ssh_test_helper::SshTestServer;

#[test]
#[ignore = "requires Docker"]
fn test_ssh_sync() {
    // Start ephemeral SSH server with auto-generated keys
    let server = SshTestServer::start().expect("SSH server should start");

    // Use server.ssh_target() for connections
    let target = server.ssh_target();
    // ... run SSH-based tests ...

    // Container auto-cleaned on drop
}
```

**Running SSH tests:**
```bash
# Build SSH test image first
docker build -t cass-ssh-test:latest -f tests/docker/Dockerfile.sshd tests/docker/

# Run ignored tests (requires Docker)
cargo test -- --ignored
```

### PTY-Based TUI Testing

TUI tests support two modes:

1. **Headless mode** (`--once` + `TUI_HEADLESS=1`):
   - Non-interactive, exits immediately after init
   - Validates launch, exit codes, no panics
   - Used in `tests/tui_smoke.rs`, `tests/tui_headless_smoke.rs`

2. **Flow simulation** (via CLI equivalents):
   - Search/filter/export flows simulated via CLI commands
   - Results compared against TUI behavior expectations
   - Artifacts captured under `test-results/e2e/tui/`
   - Used in `tests/e2e_tui_smoke_flows.rs`

**Example headless test:**
```rust
cargo_bin_cmd!("cass")
    .arg("tui")
    .arg("--once")
    .arg("--data-dir")
    .arg(&data_dir)
    .env("TUI_HEADLESS", "1")
    .assert()
    .success();
```

**Artifacts:** TUI E2E tests capture:
- Per-step stdout/stderr
- Search results as JSON
- Timing metrics
- Summary JSON with trace ID

Stored in: `test-results/e2e/tui/<trace_id>_*.txt`

**Note:** Full PTY-based interactive testing (sending real keystrokes) would require
a PTY crate (e.g., `portable_pty`). Current tests use headless mode with CLI equivalents.

---

## Running Tests

### Local Development

```bash
# Run all tests
cargo test

# Run specific test file
cargo test --test connector_claude

# Run with logging
RUST_LOG=debug cargo test

# Skip expensive tests
cargo test --lib  # Unit tests only
```

### CI Pipeline

The full CI pipeline runs:

```bash
./scripts/validate_ci.sh
```

Which includes:
1. No-mock policy check
2. `cargo fmt --check`
3. `cargo clippy`
4. `cargo test`
5. Crypto vector tests
6. `cargo audit` (if installed)

### Browser E2E Tests

**Do not run locally** - they consume significant resources.

Push to a branch and let GitHub Actions run them:
- Workflow: `.github/workflows/browser-tests.yml`
- Runs on: Chromium, Firefox, WebKit
- Uploads: Test artifacts and reports

---

## Snapshot Baseline Regeneration & Review (FrankenTUI)

Snapshot baselines are a **UI contract**, not a convenience artifact. Never run blanket blesses without proving why each change is intentional.

### Snapshot Suites and Ownership

| Snapshot suite | Test entrypoint | Snapshot files |
|----------------|-----------------|----------------|
| Core ftui harness smoke baselines | `tests/ftui_harness_snapshots.rs` (`ftui_snapshot_*`) | `tests/snapshots/ftui_*.snap` / `tests/snapshots/ftui_*.ansi.snap` |
| Cass affordance baselines | `src/ui/app.rs` (`snapshot_baseline_*`) | `tests/snapshots/cassapp_baseline_*.snap` |
| Search-surface regressions | `src/ui/app.rs` (`snapshot_search_surface_*`) | `tests/snapshots/cassapp_search_surface_*.snap` |

### Default Verification (No Regeneration)

Run these first to confirm whether snapshots are stale:

```bash
cargo test snapshot_baseline_ -- --nocapture
cargo test snapshot_search_surface_ -- --nocapture
cargo test --test ftui_harness_snapshots -- --nocapture
```

### Targeted Regeneration (Intentional Changes Only)

Use `BLESS=1` only for the suite you intentionally changed:

```bash
BLESS=1 cargo test snapshot_baseline_ -- --nocapture
BLESS=1 cargo test snapshot_search_surface_ -- --nocapture
BLESS=1 cargo test --test ftui_harness_snapshots -- --nocapture
```

### Mandatory Review Protocol

1. Regenerate only targeted snapshots (never all suites by default).
2. Inspect diffs directly:
   ```bash
   git diff -- tests/snapshots/*.snap
   ```
3. Validate behavior guards still pass:
   ```bash
   cargo test search_surface_interaction_matrix_enter_click_escape -- --nocapture
   cargo test detail_markdown_ -- --nocapture
   cargo test markdown_theme_ -- --nocapture
   ```
4. Run project quality gates:
   ```bash
   cargo fmt --check
   cargo check --all-targets
   cargo clippy --all-targets -- -D warnings
   ```
5. In the bead/PR note, explicitly record:
   - Which snapshot suite was regenerated
   - Why each changed region is intentional
   - Which non-snapshot behavioral tests were run as guards

### Snapshot Diff Heuristics for Reviewers

- **Expected:** spacing, border glyph, color-role, and layout shifts that map to an intentional UI change in the bead scope.
- **Suspicious:** missing footer/help hints, removed role gutters, vanished filter pill hierarchy, clipped text, or theme-specific parity drift (dark vs light mismatch).
- **Reject if present:** unrelated snapshot churn outside touched feature scope, unexplained multiline reflow across untouched panes, or regressions in interaction guard tests.

---

## Coverage Policy

### Threshold Requirements

| Metric | Threshold | Enforcement |
|--------|-----------|-------------|
| Line coverage | **60%** minimum | Required on PR merge |
| Target coverage | **80%** | Recommended, shown in CI summary |

### CI Enforcement

Coverage is enforced via `.github/workflows/coverage.yml`:

- **On PRs**: Coverage below 60% **blocks merge**
- **On main**: Coverage is reported to Codecov for tracking
- **Summary**: Each run shows coverage status in GitHub Actions summary

### Running Coverage Locally

```bash
# Install cargo-llvm-cov (requires nightly)
rustup install nightly
cargo +nightly install cargo-llvm-cov

# Generate coverage report
cargo +nightly llvm-cov --workspace --lib \
  --ignore-filename-regex "(tests/|benches/)"

# Generate HTML report for detailed analysis
cargo +nightly llvm-cov --workspace --lib \
  --ignore-filename-regex "(tests/|benches/)" \
  --html --open
```

### Coverage Exclusions

The following are excluded from coverage calculation:
- `tests/` directory (test code itself)
- `benches/` directory (benchmark code)

### Improving Coverage

When adding new code:

1. **Write tests first** (TDD) or alongside implementation
2. **Focus on branches**: Cover error paths, not just happy paths
3. **Use fixtures**: Real data from `tests/fixtures/` over synthetic data
4. **Check locally**: Run coverage before pushing to catch gaps early

When coverage drops on a PR:

1. Identify uncovered lines in the HTML report
2. Add targeted tests for new code paths
3. Consider if untested code is dead code (remove it)

---

## E2E Logging Infrastructure

### Unified JSONL Schema

All E2E test infrastructure emits structured JSONL logs following a unified schema. This enables consistent log aggregation, CI integration, and debugging across all test runners.

**Schema Documentation:** `test-results/e2e/SCHEMA.md`

**Event Types:**
- `run_start` - Test run begins, captures environment metadata
- `test_start` - Individual test begins
- `test_end` - Individual test completes (with status, duration, errors)
- `run_end` - Test run completes with summary statistics
- `log` - General log messages (INFO, WARN, ERROR, DEBUG)
- `phase_start`/`phase_end` - Multi-phase run tracking

**Test fields (test_start/test_end):**
- `test.name`, `test.suite`
- `test.test_id` (stable `${suite}::${name}`)
- `test.trace_id` (matches CLI trace spans)
- `test.artifact_paths` (dir/stdout/stderr/cass_log/trace)

### Logger Implementations

| Runner | Implementation | Output |
|--------|---------------|--------|
| Rust E2E | `tests/util/e2e_log.rs` | `test-results/e2e/<suite>/<test>/cass.log` |
| Shell scripts | `scripts/lib/e2e_log.sh` | `test-results/e2e/shell_*.jsonl` |
| Playwright | `tests/e2e/reporters/jsonl-reporter.ts` | `test-results/e2e/playwright_*.jsonl` |

**Per-test artifacts (Rust E2E):**
`test-results/e2e/<suite>/<test>/` contains:
- `stdout` / `stderr` - Captured command output
- `cass.log` - Structured JSONL events (SCHEMA.md)
- `trace.jsonl` - CLI trace spans (command, args, timestamps, exit_code, trace_id)

Rust E2E tests set `CASS_TRACE_FILE` + `CASS_TRACE_ID` per test to ensure trace spans
are correlated with the same `trace_id` recorded in `cass.log`.

### Rust E2E Logger

```rust
use crate::util::e2e_log::E2eLogger;

let logger = E2eLogger::new("my_test", None)?;
logger.run_start(None)?;

logger.test_start("test_name", "suite_name", Some("file.rs"), Some(42))?;
// ... run test ...
logger.test_pass("test_name", "suite_name", duration_ms)?;

logger.run_end(total, passed, failed, skipped, duration_ms)?;
```

### Shell Script Logger

```bash
source scripts/lib/e2e_log.sh

e2e_init "shell" "my_script"
e2e_run_start

e2e_test_start "test_name" "suite_name"
# ... run test ...
e2e_test_pass "test_name" "suite_name" "$duration_ms"

e2e_run_end "$total" "$passed" "$failed" "$skipped" "$duration_ms"
```

### Orchestrated E2E Runner

The unified test runner executes all E2E suites and produces consolidated reports:

```bash
# Run all E2E suites
./scripts/tests/run_all.sh

# Run specific suites
./scripts/tests/run_all.sh --rust-only
./scripts/tests/run_all.sh --shell-only
./scripts/tests/run_all.sh --playwright-only

# Control options
./scripts/tests/run_all.sh --fail-fast   # Stop on first failure
./scripts/tests/run_all.sh --verbose     # Show detailed output
```

**Outputs:**
- `test-results/e2e/<suite>/<test>/cass.log` - Per-test JSONL logs (Rust E2E)
- `test-results/e2e/*.jsonl` - Per-suite JSONL logs (shell/playwright/orchestrator)
- `test-results/e2e/combined.jsonl` - Aggregated JSONL (excludes trace.jsonl)
- `test-results/e2e/summary.md` - Human-readable Markdown summary

**Retention:**
- CI keeps E2E artifacts (logs/traces/summary) for 7 days by default.
- Local `test-results/e2e/` can be cleaned manually when no longer needed.

### Parsing JSONL Logs

```bash
# Count failures across all suites
jq -s '[.[] | select(.event == "test_end" and .result.status == "fail")] | length' \
  $(find test-results/e2e -type f \( -name "*.jsonl" -o -name "cass.log" \) \
    ! -name "trace.jsonl" ! -name "combined.jsonl")

# Get failed test names
jq -r 'select(.event == "test_end" and .result.status == "fail") | .test.name' \
  $(find test-results/e2e -type f \( -name "*.jsonl" -o -name "cass.log" \) \
    ! -name "trace.jsonl" ! -name "combined.jsonl")

# Duration by runner
jq -s 'group_by(.runner) | map({runner: .[0].runner, total_ms: [.[] | select(.event == "run_end") | .summary.duration_ms] | add})' \
  $(find test-results/e2e -type f \( -name "*.jsonl" -o -name "cass.log" \) \
    ! -name "trace.jsonl" ! -name "combined.jsonl")
```

### JSONL Schema Validator

The `validate-e2e-jsonl.sh` script validates E2E log files conform to the expected schema:

```bash
# Validate all E2E JSONL logs
./scripts/validate-e2e-jsonl.sh test-results/e2e/*.jsonl test-results/e2e/**/cass.log

# Validate a specific file
./scripts/validate-e2e-jsonl.sh test-results/e2e/e2e_cli_flows/search_basic_returns_valid_json/cass.log
```

**Validation checks:**
- Required fields: `ts`, `run_id`, `runner` on all events
- Event-specific fields:
  - `run_start`: requires `env`
  - `test_start`/`test_end`: requires `test.name`
  - `test_end`: requires `result.status`
  - `run_end`: requires `summary`
  - `phase_start`/`phase_end`: requires `phase.name`
  - `metrics`: requires `metrics`
- Structural checks:
  - `test_start` count matches `test_end` count
  - `run_start` present if tests exist

**CI Integration:**
The validator runs automatically in CI after E2E tests. Schema violations fail the build with actionable error messages like:
```
file.jsonl:15: Event 'test_end' missing required field 'result.status'
```

---

## Test Reports

Generated reports go in `test-results/`:

| File | Description |
|------|-------------|
| `no_mock_audit.md` | Mock pattern audit results |
| `no_mock_allowlist.json` | Approved mock exceptions |
| `e2e/SCHEMA.md` | E2E logging schema documentation |
| `e2e/<suite>/<test>/` | Per-test artifacts (stdout/stderr/cass.log/trace.jsonl) |
| `e2e/*.jsonl` | Per-suite JSONL logs |
| `e2e/combined.jsonl` | Aggregated JSONL from all suites |
| `e2e/summary.md` | Human-readable E2E summary |

---

## Adding New Tests

### Checklist

When adding tests:

- [ ] Uses real fixtures, not mock data
- [ ] Follows existing test patterns
- [ ] Runs fast (< 1s for unit, < 10s for integration)
- [ ] Has clear failure messages
- [ ] Documented if non-obvious

### Test Naming

```rust
// Good: Descriptive and specific
#[test]
fn parse_claude_session_with_tool_calls_extracts_all_snippets() { }

// Bad: Vague
#[test]
fn test_parsing() { }
```

---

## Related Documentation

- `AGENTS.md` - Agent guidelines (E2E browser test policy)
- `test-results/no_mock_audit.md` - Current mock audit
- `test-results/no_mock_allowlist.json` - Approved exceptions
- `test-results/e2e/SCHEMA.md` - Unified E2E logging schema
- `scripts/tests/run_all.sh` - Orchestrated E2E runner
- `scripts/lib/e2e_log.sh` - Shell E2E logging library
- `tests/util/e2e_log.rs` - Rust E2E logging module
- `tests/e2e/reporters/jsonl-reporter.ts` - Playwright JSONL reporter
- `.github/workflows/` - CI workflow definitions

---

*Last updated: 2026-02-09*
