# Coverage Policy: coding_agent_session_search (cass)

**Bead**: coding_agent_session_search-2r76
**Last Updated**: 2026-01-27

---

## 1. Executive Summary

This document defines explicit coverage targets, phased threshold increases, and justified exclusions for the cass codebase. The goal is to systematically improve test coverage while maintaining development velocity.

---

## 2. Current Baseline

| Metric | Value | Date |
|--------|-------|------|
| Line Coverage | 58.33% | 2026-01-27 |
| Function Coverage | ~55% | 2026-01-27 |
| Total Tests | ~2,100+ | 2026-01-27 |

### Coverage by Module (Approximate)

| Module | Coverage | Status |
|--------|----------|--------|
| connectors/ | 85%+ | Excellent |
| search/ | 75%+ | Good |
| storage/ | 70%+ | Good |
| sources/ | 65%+ | Adequate |
| ui/ | 60%+ | Adequate |
| pages/ | 55%+ | Needs Work |
| indexer/ | 50%+ | Needs Work |
| lib.rs | <30% | Critical Gap |
| model/ | <20% | Critical Gap |

---

## 3. Phased Coverage Targets

### Phase 1: Foundation (Current - Q1 2026)
- **Target**: 60% line coverage (current CI gate)
- **Focus**: Maintain existing coverage, close critical gaps
- **Status**: ACHIEVED

### Phase 2: Stability (Q2 2026)
- **Target**: 70% line coverage
- **Focus**:
  - lib.rs unit tests (argument parsing, error types)
  - model/ serialization tests
  - encryption.rs expanded tests
- **Blockers**: None

### Phase 3: Confidence (Q3 2026)
- **Target**: 80% line coverage
- **Focus**:
  - indexer/ comprehensive tests
  - pages/ export format tests
  - Error path coverage
- **Blockers**: Phase 2 completion

### Phase 4: Excellence (Q4 2026)
- **Target**: 90% line coverage
- **Focus**:
  - Edge case completeness
  - Performance regression tests
  - Security path hardening
- **Blockers**: Phase 3 completion

---

## 4. CI Enforcement

### Current Configuration
```yaml
# .github/workflows/coverage.yml
THRESHOLD=60  # Phase 1 target
```

### Threshold Schedule
| Date | Threshold | Notes |
|------|-----------|-------|
| 2026-01-27 | 60% | Current |
| 2026-04-01 | 65% | Mid-Phase 2 |
| 2026-07-01 | 70% | Phase 2 Complete |
| 2026-10-01 | 80% | Phase 3 Complete |
| 2027-01-01 | 85% | Phase 4 Progress |
| 2027-04-01 | 90% | Phase 4 Complete |

### Enforcement Rules
1. **PRs**: Must not decrease coverage below threshold
2. **Main branch**: Coverage report uploaded to Codecov
3. **Releases**: Must meet current phase target

---

## 5. Justified Exclusions

Certain code paths are intentionally excluded from coverage requirements. These must be documented and reviewed quarterly.

### Permanently Excluded

| Path Pattern | Reason | Review Date |
|--------------|--------|-------------|
| `tests/` | Test code itself | N/A |
| `benches/` | Benchmark code | N/A |
| `build.rs` | Build script (compile-time only) | N/A |
| `src/bin/` | Binary entry points (covered by E2E) | 2026-07-01 |

### Temporarily Excluded (To Be Covered)

| Path Pattern | Reason | Target Date |
|--------------|--------|-------------|
| `src/pages/wizard.rs` | Complex UI flow, needs E2E | 2026-07-01 |
| `src/ui/tui.rs:8000+` | TUI rendering (covered by snapshots) | 2026-10-01 |
| `src/html_export/` | Template rendering (needs fixture tests) | 2026-07-01 |

### Platform-Specific Code

| Path Pattern | Reason |
|--------------|--------|
| `#[cfg(target_os = "macos")]` | macOS-only (keychain access) |
| `#[cfg(windows)]` | Windows-specific paths |

---

## 6. Coverage Improvement Workflow

### For Contributors

1. **Before submitting PR**:
   ```bash
   # Run coverage through rch
   rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --ignore-filename-regex "(tests/|benches/)"
   ```

2. **Check coverage delta**:
   ```bash
   # Compare with main branch
   rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --json > coverage.json
   jq '.data[0].totals.lines.percent' coverage.json
   ```

3. **If coverage drops**: Add tests for new code before submitting

### For Maintainers

1. **Quarterly review**: Check exclusion list for stale entries
2. **Phase transitions**: Update CI threshold on schedule
3. **Gap reports**: Generate module-level coverage reports monthly

---

## 7. Tools and Commands

### Generate Coverage Report
```bash
# Full HTML report
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --html \
  --ignore-filename-regex "(tests/|benches/)"

# JSON summary
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --json \
  --ignore-filename-regex "(tests/|benches/)" \
  > coverage.json

# Codecov format
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --codecov \
  --ignore-filename-regex "(tests/|benches/)" \
  > codecov.json
```

### Find Uncovered Lines
```bash
# Show uncovered regions
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --show-missing-lines \
  --ignore-filename-regex "(tests/|benches/)"

# Uncovered functions
scripts/coverage-uncovered.sh
```

### Per-Module Coverage
```bash
# Filter to specific module
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-coverage-target cargo llvm-cov --lib --json | \
  jq '.data[0].files[] | select(.filename | contains("connectors"))'
```

---

## 8. Priority Gaps (from test-coverage-audit.md)

### P0 - Critical (Blocks Releases)
- [ ] lib.rs unit tests - Argument parsing, error types, command dispatch
- [ ] model/ unit tests - Core type serialization/conversion

### P1 - High (Next Sprint)
- [ ] encryption.rs expanded tests - More vectors, error paths
- [ ] update_check.rs integration tests - Version comparison, network errors
- [ ] Large archive performance tests - 10K+ conversation benchmarks

### P2 - Medium (Backlog)
- [ ] Migration rollback tests - Storage schema downgrade paths
- [ ] TUI interaction coverage - More keyboard/mouse event tests
- [ ] Error message quality tests - User-facing error strings

### P3 - Low (Nice to Have)
- [ ] Bookmarks module external tests
- [ ] Indexer edge case coverage
- [ ] Export format fidelity tests

---

## 9. Monitoring and Reporting

### Dashboards
- **Codecov**: https://codecov.io/gh/Dicklesworthstone/coding_agent_session_search
- **GitHub Actions**: Coverage workflow runs on every PR

### Alerts
- Coverage drops >2% trigger PR comment
- Coverage below threshold blocks merge

### Reports
- Weekly: Coverage trend in team Slack
- Monthly: Module-level gap report
- Quarterly: Exclusion list review

---

## 10. FAQ

### Q: Why 90% instead of 100%?
A: Diminishing returns. The last 10% typically involves:
- Platform-specific code we can't test in CI
- Error paths triggered by hardware failures
- UI rendering code better tested via snapshots

### Q: Can I add code without tests?
A: Only if:
1. It's in an excluded category (see Section 5)
2. You create a follow-up bead to add tests
3. Total coverage doesn't drop below threshold

### Q: How do I exclude a file from coverage?
A: Add to the `--ignore-filename-regex` pattern in:
- `.github/workflows/coverage.yml`
- This document's exclusion table

### Q: What if CI fails due to coverage?
A: Either:
1. Add tests for your new code
2. If justified, propose an exclusion with rationale

---

## 11. Revision History

| Date | Version | Changes |
|------|---------|---------|
| 2026-01-27 | 1.0 | Initial policy (br-2r76) |

---

*This policy was created as part of bead coding_agent_session_search-2r76 to define coverage targets and phased threshold increases.*
