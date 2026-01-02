---
status: pending
priority: p3
issue_id: "005"
tags: [code-review, refactoring, complexity]
dependencies: []
---

# P3: Function Complexity - parse_composer_data is 163 Lines

## Problem Statement

The `parse_composer_data()` function is 163 lines long and handles 7 different responsibilities:
1. JSON parsing
2. Composer ID extraction
3. Deduplication checking
4. Message parsing (multiple formats)
5. Title extraction
6. Workspace extraction
7. Conversation construction

Additionally, it takes 7 parameters which makes it harder to test and maintain.

**Why it matters:** Long functions with many responsibilities are harder to understand, test, and modify.

## Findings

**Source:** Pattern Recognition Specialist, Architecture Strategist

**Location:** `src/connectors/cursor.rs`, lines 485-648

```rust
fn parse_composer_data(
    key: &str,
    value: &str,
    db_path: &Path,
    _since_ts: Option<i64>,  // Unused but kept for interface consistency
    seen_ids: &mut HashSet<String>,
    bubble_data: &BubbleDataMap,
    workspace: Option<&PathBuf>,
) -> Option<NormalizedConversation>
```

## Proposed Solutions

### Option A: Extract Helper Functions
**Pros:** Clearer separation of concerns, easier to test
**Cons:** More functions to navigate
**Effort:** Medium
**Risk:** Low

```rust
// Extract these helpers:
fn parse_messages_from_new_format(...) -> Vec<NormalizedMessage>
fn parse_messages_from_tabs(...) -> Vec<NormalizedMessage>
fn parse_messages_from_conversation_map(...) -> Vec<NormalizedMessage>
fn extract_title(val: &Value, messages: &[NormalizedMessage]) -> Option<String>
```

### Option B: Use ParseContext Struct
**Pros:** Reduces parameter count, cleaner API
**Cons:** More indirection
**Effort:** Medium
**Risk:** Low

```rust
struct ParseContext<'a> {
    db_path: &'a Path,
    seen_ids: &'a mut HashSet<String>,
    bubble_data: &'a BubbleDataMap,
    workspace: Option<&'a PathBuf>,
}
```

### Option C: Keep Current (Works)
**Pros:** No refactoring risk
**Cons:** Technical debt remains
**Effort:** None
**Risk:** None

## Recommended Action

_To be filled during triage_

## Technical Details

**Affected Files:**
- `src/connectors/cursor.rs` (lines 485-648)

**Metrics:**
- Current: 163 lines, 7 parameters
- Goal: <50 lines per function, <4 parameters

## Acceptance Criteria

- [ ] Main function <50 lines
- [ ] Each helper has single responsibility
- [ ] All existing tests pass
- [ ] No change in behavior

## Work Log

| Date | Action | Outcome/Learning |
|------|--------|------------------|
| 2026-01-02 | Identified by Architecture Strategist | Function too long |

## Resources

- PR #26: https://github.com/Dicklesworthstone/coding_agent_session_search/pull/26
