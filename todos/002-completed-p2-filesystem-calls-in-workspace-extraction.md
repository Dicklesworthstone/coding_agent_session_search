---
status: completed
priority: p2
issue_id: "002"
tags: [code-review, performance]
dependencies: []
---

# P2: Filesystem Calls in Workspace Extraction Hot Path

## Problem Statement

The `extract_workspace_from_context()` function makes `is_file()` and `exists()` syscalls for each path when computing the common ancestor directory. These filesystem calls are unnecessary since we only need to compute the common path prefix from strings.

**Why it matters:** Each syscall adds latency (~1-5ms). With 100 file references in a conversation, this adds 100-500ms per conversation during indexing.

## Findings

**Source:** Performance Oracle Agent, Code Simplicity Reviewer

**Location:** `src/connectors/cursor.rs`, lines 364-376

```rust
let mut common = if first_path.is_file() || !first_path.exists() {  // syscall
    first_path.parent()?.to_path_buf()
} else {
    first_path.clone()
};

for path in paths.iter().skip(1) {
    let path_to_check = if path.is_file() || !path.exists() {  // syscall per path
        path.parent().map(PathBuf::from)
    } else {
        Some(path.clone())
    };
    // ...
}
```

**Evidence:**
- `is_file()` and `exists()` are syscalls that hit the filesystem
- Called for every path in fileSelections, folderSelections, etc.
- Some conversations have 100+ file references
- Paths come from JSON data - no need to verify they exist on disk

## Proposed Solutions

### Option A: Pure String-Based Path Comparison (Recommended)
**Pros:** No I/O, fast, predictable performance
**Cons:** May treat all paths as files (take parent dir)
**Effort:** Low
**Risk:** Low

```rust
fn extract_workspace_from_context(val: &Value) -> Option<PathBuf> {
    let paths = Self::collect_file_paths(val);
    if paths.is_empty() { return None; }

    // Assume all paths are files, use parent directories
    let mut common: Vec<_> = paths[0].parent()?.components().collect();
    for path in &paths[1..] {
        if let Some(parent) = path.parent() {
            let components: Vec<_> = parent.components().collect();
            common.truncate(
                common.iter().zip(&components)
                    .take_while(|(a, b)| a == b)
                    .count()
            );
        }
    }
    (common.len() > 2).then(|| common.into_iter().collect())
}
```

### Option B: Heuristic-Based (No Syscalls)
**Pros:** More accurate file vs directory detection
**Cons:** Slightly more complex
**Effort:** Low
**Risk:** Low

Check if path has file extension to determine if it's a file, no syscalls needed.

## Recommended Action

_To be filled during triage_

## Technical Details

**Affected Files:**
- `src/connectors/cursor.rs` (lines 364-408)

**Components:**
- `extract_workspace_from_context()` function

## Acceptance Criteria

- [ ] No filesystem syscalls in workspace extraction
- [ ] Workspace extraction produces same results for test cases
- [ ] All existing tests pass

## Work Log

| Date | Action | Outcome/Learning |
|------|--------|------------------|
| 2026-01-02 | Identified by Performance Oracle agent | Syscalls in hot path |

## Resources

- PR #26: https://github.com/Dicklesworthstone/coding_agent_session_search/pull/26
