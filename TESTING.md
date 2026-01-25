# Testing Guide

This document describes the test infrastructure for `cass` (Coding Agent Session Search).

## Test Matrix

### Unit Tests (in-source)

Located within `src/**/*.rs` in `#[cfg(test)]` modules:

| Module | Coverage | Description |
|--------|----------|-------------|
| `src/connectors/*.rs` | High | Session parsing for each agent type |
| `src/search/query.rs` | High | Query parsing, boolean operators, wildcards |
| `src/search/tantivy.rs` | Medium | Tantivy schema and indexing |
| `src/indexer/mod.rs` | High | Indexer orchestration, provenance injection |
| `src/sources/config.rs` | High | Source configuration, path mappings |
| `src/ui/*.rs` | Medium | TUI rendering, themes, components |

Run unit tests:
```bash
cargo test --lib
```

### Integration Tests

Located in `tests/*.rs`:

| File | Type | Description |
|------|------|-------------|
| `connector_aider.rs` | Integration | Aider session parsing (39 tests) |
| `connector_amp.rs` | Integration | AMP session parsing (24 tests) |
| `connector_claude.rs` | Integration | Claude Code parsing (21 tests) |
| `connector_cline.rs` | Integration | Cline parsing (25 tests) |
| `connector_codex.rs` | Integration | Codex parsing (26 tests) |
| `connector_gemini.rs` | Integration | Gemini parsing (17 tests) |
| `connector_opencode.rs` | Integration | OpenCode parsing (15 tests) |
| `connector_pi_agent.rs` | Integration | Pi-Agent parsing (17 tests) |
| `fs_errors.rs` | Integration | Filesystem error handling (18 tests) |
| `parse_errors.rs` | Integration | Parser error handling (18 tests) |
| `storage.rs` | Integration | SQLite storage (44 tests) |
| `indexer_tantivy.rs` | Integration | Tantivy indexing (5 tests) |
| `search_caching.rs` | Integration | Search result caching (2 tests) |
| `search_filters.rs` | Integration | Filter application (3 tests) |
| `search_wildcard_fallback.rs` | Integration | Wildcard fallback (2 tests) |
| `logging.rs` | Integration | Log configuration (3 tests) |
| `ranking.rs` | Integration | Search ranking algorithms (7 tests) |
| `concurrent_search.rs` | Integration | Concurrent search operations (6 tests) |

Run integration tests:
```bash
cargo test --test <test_name>
# Example: cargo test --test connector_claude
```

### End-to-End Tests

Test full CLI workflows with real I/O:

| File | Type | Description |
|------|------|-------------|
| `e2e_cli_flows.rs` | E2E | CLI command flows (20 tests) |
| `e2e_filters.rs` | E2E | Filter combinations (8 tests) |
| `e2e_index_tui.rs` | E2E | Index + TUI headless (1 test) |
| `e2e_install_easy.rs` | E2E | Easy installation (1 test) |
| `e2e_multi_connector.rs` | E2E | Multi-connector indexing (8 tests) |
| `e2e_search_index.rs` | E2E | Search/index integration (15 tests) |
| `e2e_sources.rs` | E2E | Remote sources workflow (22 tests) |
| `multi_source_integration.rs` | E2E | Multi-source integration (14 tests) |
| `install_scripts.rs` | E2E | Install script validation (4 tests) |
| `watch_e2e.rs` | E2E | Watch mode behavior (4 tests) |

E2E tests often require `--test-threads=1`:
```bash
cargo test --test e2e_index_tui -- --test-threads=1
```

### Browser E2E (Playwright)

Playwright-based tests validate web viewer and HTML export flows.

Install dependencies (one-time):
```bash
npx playwright install --with-deps
```

Run HTML export WebCrypto decryption tests:
```bash
npx playwright test tests/html_export/html_export_encryption.test.js
```

### CLI/Robot Tests

Test robot mode and CLI contracts:

| File | Type | Description |
|------|------|-------------|
| `cli_index.rs` | CLI | Index command tests (6 tests) |
| `cli_robot.rs` | CLI | Robot mode output (137 tests) |
| `robot_perf.rs` | Perf | Robot mode performance |
| `regression_behavioral.rs` | Regression | Behavioral contracts (21 tests) |

### UI/Snapshot Tests

| File | Type | Description |
|------|------|-------------|
| `ui_snap.rs` | Snapshot | TUI rendering snapshots (50 tests) |
| `ui_components.rs` | Unit | Component behavior (3 tests) |
| `ui_footer.rs` | Unit | Footer rendering (1 test) |
| `ui_help.rs` | Unit | Help display (1 test) |
| `ui_hotkeys.rs` | Unit | Hotkey handling (2 tests) |

## Running Tests

### All Tests
```bash
cargo test
```

### Specific Test File
```bash
cargo test --test <filename_without_rs>
# Example: cargo test --test e2e_filters
```

### Specific Test Function
```bash
cargo test <test_name>
# Example: cargo test parse_boolean_query
```

### Tests Matching Pattern
```bash
cargo test <pattern>
# Example: cargo test connector_  # All connector tests
# Example: cargo test e2e_        # All e2e tests
```

### With Output
```bash
cargo test -- --nocapture
```

### Single-Threaded (for E2E)
```bash
cargo test --test <test_name> -- --test-threads=1
```

## Coverage

Generate coverage report using `cargo-llvm-cov`:

```bash
# Install (one-time)
cargo install cargo-llvm-cov

# Generate text summary
cargo llvm-cov --all-features --workspace --text

# Generate lcov.info (for codecov/coveralls)
cargo llvm-cov --all-features --workspace --lcov --output-path lcov.info

# Generate HTML report
cargo llvm-cov --all-features --workspace --html
# Open target/llvm-cov/html/index.html

# Ignore test files in coverage
cargo llvm-cov --all-features --workspace \
  --ignore-filename-regex='(tests/|benches/|\.cargo/)' \
  --text
```

## Trace Files & Logs

### Trace File Location
Tests that use `--trace-file` write to:
- `/tmp/cass-trace-*.json` (temporary)
- `test-artifacts/traces/` (CI artifacts)

### Enabling Trace Output
```bash
# Run with tracing enabled
RUST_LOG=debug cargo test <test_name> -- --nocapture
```

### CI Artifacts
CI uploads these artifacts:
- `test-artifacts-e2e/traces/` - Trace files from E2E runs
- `test-artifacts-e2e/logs/` - Run summaries
- `coverage-report/lcov.info` - Coverage data
- `coverage-report/coverage-summary.txt` - Human-readable coverage

## Robot Mode / Introspect-Contract Tests

Tests in `cli_robot.rs` verify the robot mode contract:

```bash
# Run all robot contract tests
cargo test --test cli_robot

# Run specific introspect tests
cargo test --test cli_robot introspect

# Run capability contract tests
cargo test --test cli_robot capabilities
```

Key test categories:
- `test_robot_search_*` - Search output format
- `test_robot_help_*` - Help text contracts
- `test_capabilities_*` - Capability discovery
- `test_introspect_*` - Internal state inspection

## Fixture Files

Test fixtures are in `tests/fixtures/`:
- Session files for connector tests
- Golden output files for regression tests

## Adding New Tests

### Unit Test (in module)
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature() {
        // ...
    }
}
```

### Integration Test (tests/*.rs)
```rust
// tests/my_feature.rs
use coding_agent_search::*;

#[test]
fn test_integration() {
    // ...
}
```

### E2E Test
```rust
// tests/e2e_my_flow.rs
use assert_cmd::Command;

#[test]
fn test_e2e_flow() {
    let mut cmd = Command::cargo_bin("cass").unwrap();
    cmd.arg("search").arg("test").arg("--robot");
    cmd.assert().success();
}
```

## Beads (Issue Tracking)

Test-related beads for reference:

| Bead | Description |
|------|-------------|
| `tst` | Test Coverage Epic |
| `tst.cli` | CLI Command Tests |
| `tst.idx` | Indexer/Tantivy Tests |
| `tst.err` | Error Handling Tests |
| `tst.inf` | Test Infrastructure |
| `tst.e2e.*` | E2E Test subtasks |
| `tst.con.*` | Connector test subtasks |
| `bs8` | CI wiring: coverage + logs |
| `ke5` | Test documentation (this doc) |

## CI Pipeline

The CI (`.github/workflows/ci.yml`) runs:

1. **check** job: fmt, clippy, tests, benches, UBS
2. **e2e** job: E2E tests, artifact collection
3. **coverage** job: llvm-cov coverage report

See README.md "CI Pipeline & Artifacts" section for artifact details.
