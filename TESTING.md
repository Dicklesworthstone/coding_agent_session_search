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

Some test scenarios require mock implementations. These are explicitly allowlisted:

**Allowlisted patterns** (see `test-results/no_mock_allowlist.json`):

1. **Trait abstraction tests** (`#[cfg(test)]` only):
   - `MockEmbedder` in `src/search/embedder.rs` - tests Embedder trait contract
   - `MockReranker` in `src/search/reranker.rs` - tests Reranker trait contract
   - `MockDaemon` in `src/search/daemon_client.rs` - tests daemon retry logic

2. **Integration test harnesses**:
   - `ChannelDaemonClient` - real channel communication, not a mock

3. **Feature functionality** (not test infrastructure):
   - `src/pages/redact.rs` - privacy feature that replaces usernames

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

### Logger Implementations

| Runner | Implementation | Output |
|--------|---------------|--------|
| Rust E2E | `tests/util/e2e_log.rs` | `test-results/e2e/rust_*.jsonl` |
| Shell scripts | `scripts/lib/e2e_log.sh` | `test-results/e2e/shell_*.jsonl` |
| Playwright | `tests/e2e/reporters/jsonl-reporter.ts` | `test-results/e2e/playwright_*.jsonl` |

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
- `test-results/e2e/*.jsonl` - Per-suite JSONL logs
- `test-results/e2e/combined.jsonl` - Aggregated JSONL from all suites
- `test-results/e2e/summary.md` - Human-readable Markdown summary

### Parsing JSONL Logs

```bash
# Count failures across all suites
jq -s '[.[] | select(.event == "test_end" and .result.status == "fail")] | length' \
  test-results/e2e/*.jsonl

# Get failed test names
jq -r 'select(.event == "test_end" and .result.status == "fail") | .test.name' \
  test-results/e2e/*.jsonl

# Duration by runner
jq -s 'group_by(.runner) | map({runner: .[0].runner, total_ms: [.[] | select(.event == "run_end") | .summary.duration_ms] | add})' \
  test-results/e2e/*.jsonl
```

### JSONL Schema Validator

The `validate-e2e-jsonl.sh` script validates E2E log files conform to the expected schema:

```bash
# Validate all E2E JSONL logs
./scripts/validate-e2e-jsonl.sh test-results/e2e/*.jsonl

# Validate a specific file
./scripts/validate-e2e-jsonl.sh test-results/e2e/rust_20260127_044815.jsonl
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

*Last updated: 2026-01-26*
