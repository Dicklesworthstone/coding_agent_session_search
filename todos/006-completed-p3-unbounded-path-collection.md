---
status: completed
priority: p3
issue_id: "006"
tags: [code-review, performance, defensive]
dependencies: []
---

# P3: Unbounded Path Collection in Workspace Extraction

## Problem Statement

The `extract_workspace_from_context()` function collects paths from JSON arrays without any limit. Some conversations may have hundreds of file selections, leading to unbounded memory growth and unnecessary processing.

**Why it matters:** Pathological cases with 1000+ file references could cause performance issues. Once we have ~10 paths, additional paths don't meaningfully improve common ancestor accuracy.

## Findings

**Source:** Performance Oracle Agent

**Location:** `src/connectors/cursor.rs`, lines 280-355

```rust
fn extract_workspace_from_context(val: &Value) -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    // No limit on paths collected...
    for sel in selections {  // Could be 1000+ items
        // ...
        paths.push(path);
    }
}
```

## Proposed Solutions

### Option A: Add Path Limit (Recommended)
**Pros:** Simple, prevents pathological cases
**Cons:** Might miss edge cases where later paths matter
**Effort:** Very Low
**Risk:** Very Low

```rust
const MAX_PATHS_FOR_WORKSPACE: usize = 10;

for sel in selections.iter().take(MAX_PATHS_FOR_WORKSPACE) {
    // ...
}
```

### Option B: Early Termination When Confident
**Pros:** Smarter, stops when common ancestor is stable
**Cons:** More complex logic
**Effort:** Medium
**Risk:** Low

Stop collecting paths once common ancestor stops changing for N iterations.

## Recommended Action

_To be filled during triage_

## Technical Details

**Affected Files:**
- `src/connectors/cursor.rs` (lines 293-355)

## Acceptance Criteria

- [ ] Path collection limited to reasonable number (e.g., 10-20)
- [ ] Workspace extraction still accurate for normal cases
- [ ] All existing tests pass

## Work Log

| Date | Action | Outcome/Learning |
|------|--------|------------------|
| 2026-01-02 | Identified by Performance Oracle | Unbounded collection |

## Resources

- PR #26: https://github.com/Dicklesworthstone/coding_agent_session_search/pull/26
