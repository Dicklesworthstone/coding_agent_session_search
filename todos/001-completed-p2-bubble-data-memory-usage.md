---
status: completed
priority: p2
issue_id: "001"
tags: [code-review, performance, memory]
dependencies: []
---

# P2: Pre-fetching All Bubble Data Causes Memory Spike

## Problem Statement

The `fetch_bubble_data()` function loads ALL `bubbleId:*` rows into a HashMap upfront before processing any conversations. For users with 1500+ conversations (~30,000 bubble entries), this creates a ~60MB memory spike during indexing.

**Why it matters:** At scale (100x), this could consume 6GB of memory. Users with large Cursor installations may experience slow indexing or memory pressure.

## Findings

**Source:** Performance Oracle Agent

**Location:** `src/connectors/cursor.rs`, lines 172-208 and 429

```rust
// Line 429 - loads ALL bubble data before processing
let bubble_data = Self::fetch_bubble_data(&conn);
```

**Evidence:**
- `fetch_bubble_data` queries ALL `bubbleId:*` keys (~30,000 for 1500 conversations)
- Each bubble contains full JSON with message content (several KB each)
- Memory estimate: 30,000 bubbles × 2KB avg = ~60MB
- This happens before any `since_ts` filtering

**Projected Impact:**
| Conversations | Bubbles | Memory Usage | Parse Time |
|--------------|---------|--------------|------------|
| 1,500 | 30,000 | ~60MB | ~2-3s |
| 15,000 | 300,000 | ~600MB | ~20-30s |
| 150,000 | 3,000,000 | ~6GB | minutes |

## Proposed Solutions

### Option A: Lazy Loading per Conversation
**Pros:** 90% memory reduction, only loads data when needed
**Cons:** More database queries (one per conversation)
**Effort:** Medium
**Risk:** Low

```rust
fn get_bubble_data_for_composer(conn: &Connection, composer_id: &str) -> BubbleDataMap {
    let pattern = format!("bubbleId:{}:%", composer_id);
    conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE ?")
        .query_map([&pattern], |row| { ... })
}
```

### Option B: Batch Loading
**Pros:** Fewer queries than Option A, bounded memory
**Cons:** More complex implementation
**Effort:** High
**Risk:** Medium

Load bubble data in batches of 100 conversations at a time.

### Option C: Keep Current (Accept Trade-off)
**Pros:** No code changes, O(1) lookup amortizes cost
**Cons:** Memory spike remains, doesn't scale
**Effort:** None
**Risk:** None (current behavior)

## Recommended Action

_To be filled during triage_

## Technical Details

**Affected Files:**
- `src/connectors/cursor.rs` (lines 172-208, 429)

**Components:**
- `fetch_bubble_data()` function
- `extract_from_db()` function

## Acceptance Criteria

- [ ] Memory usage during indexing stays under 100MB for 1500 conversations
- [ ] Indexing time doesn't regress significantly
- [ ] All 67 existing tests pass

## Work Log

| Date | Action | Outcome/Learning |
|------|--------|------------------|
| 2026-01-02 | Identified by Performance Oracle agent | Memory spike documented |

## Resources

- PR #26: https://github.com/Dicklesworthstone/coding_agent_session_search/pull/26
- Performance Oracle analysis
