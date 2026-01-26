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

## Logging and Reports

### Structured Test Output

Tests that produce output should use JSON format:

```bash
# E2E scripts should output JSONL
./scripts/daemon/cass_daemon_e2e.sh 2>&1 | tee test-results/daemon_e2e.jsonl
```

### Test Reports

Generated reports go in `test-results/`:

- `no_mock_audit.md` - Mock pattern audit
- `no_mock_allowlist.json` - Approved exceptions
- `e2e/*.jsonl` - E2E run logs
- `e2e/summary.md` - Human-readable summary

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
- `.github/workflows/` - CI workflow definitions

---

*Last updated: 2026-01-26*
