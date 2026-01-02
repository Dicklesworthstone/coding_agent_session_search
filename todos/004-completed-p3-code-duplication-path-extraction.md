---
status: completed
priority: p3
issue_id: "004"
tags: [code-review, refactoring, dry]
dependencies: []
---

# P3: Code Duplication in Path Extraction Loops

## Problem Statement

The `extract_workspace_from_context()` function has nearly identical code repeated 5 times for extracting paths from different JSON fields: `fileSelections`, `folderSelections`, `selections`, `newlyCreatedFiles`, and `newlyCreatedFolders`.

**Why it matters:** Violates DRY principle, makes maintenance harder, ~50 lines could be reduced to ~15.

## Findings

**Source:** Pattern Recognition Specialist, Code Simplicity Reviewer

**Location:** `src/connectors/cursor.rs`, lines 293-355

```rust
// Pattern repeated 5 times with minor variations:
if let Some(selections) = context.get("fileSelections").and_then(|v| v.as_array()) {
    for sel in selections {
        if let Some(uri) = sel.get("uri")
            && let Some(path) = extract_from_uri(uri)
        {
            paths.push(path);
        }
    }
}

// Nearly identical for folderSelections, selections...
// Slightly different for newlyCreatedFiles, newlyCreatedFolders (handles string array too)
```

## Proposed Solutions

### Option A: Helper Function with Key List
**Pros:** Concise, maintainable
**Cons:** Slightly less explicit
**Effort:** Low
**Risk:** Low

```rust
fn collect_file_paths(val: &Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let ctx = val.get("context");

    // URI-based selections
    for key in ["fileSelections", "folderSelections", "selections"] {
        if let Some(arr) = ctx.and_then(|c| c.get(key)).and_then(|v| v.as_array()) {
            paths.extend(arr.iter().filter_map(|s| extract_uri_path(s.get("uri")?)));
        }
    }

    // Mixed format arrays (string or object with uri)
    for key in ["newlyCreatedFiles", "newlyCreatedFolders"] {
        if let Some(arr) = val.get(key).and_then(|v| v.as_array()) {
            paths.extend(arr.iter().filter_map(|f|
                f.as_str().map(PathBuf::from)
                    .or_else(|| extract_uri_path(f.get("uri")?))
            ));
        }
    }
    paths
}
```

### Option B: Keep Current (Explicit)
**Pros:** Very explicit, easy to understand
**Cons:** Duplicated code, harder to maintain
**Effort:** None
**Risk:** None

## Recommended Action

_To be filled during triage_

## Technical Details

**Affected Files:**
- `src/connectors/cursor.rs` (lines 293-355)

**LOC Reduction:** ~35 lines

## Acceptance Criteria

- [ ] All 5 extraction patterns consolidated into helper
- [ ] All existing tests pass
- [ ] No change in behavior

## Work Log

| Date | Action | Outcome/Learning |
|------|--------|------------------|
| 2026-01-02 | Identified by Pattern Recognition agent | DRY violation |

## Resources

- PR #26: https://github.com/Dicklesworthstone/coding_agent_session_search/pull/26
