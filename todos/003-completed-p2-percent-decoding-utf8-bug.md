---
status: completed
priority: p2
issue_id: "003"
tags: [code-review, bug, encoding]
dependencies: []
---

# P2: Custom Percent-Decoding Has UTF-8 Bug

## Problem Statement

The `parse_file_uri()` function implements manual percent-decoding that only handles single-byte ASCII characters correctly. Multi-byte UTF-8 sequences (e.g., `%E4%B8%AD` for Chinese characters) will be corrupted.

**Why it matters:** Users with non-ASCII characters in their file paths (international users, projects with Unicode names) will see garbled workspace paths.

## Findings

**Source:** Security Sentinel, Code Simplicity Reviewer, Pattern Recognition Specialist

**Location:** `src/connectors/cursor.rs`, lines 252-273

```rust
if hex.len() == 2
    && let Ok(byte) = u8::from_str_radix(&hex, 16)
{
    decoded.push(byte as char);  // BUG: treats byte as char directly
    continue;
}
```

**Evidence:**
- `byte as char` only works for ASCII (0x00-0x7F)
- For bytes 0x80-0xFF (first byte of UTF-8 sequences), this creates invalid characters
- Example: `%E4%B8%AD` (Chinese "中") becomes three invalid chars instead of one valid char

**Reproduction:**
```rust
// Input: "file:///Users/test/%E4%B8%AD%E6%96%87.txt"
// Expected: "/Users/test/中文.txt"
// Actual: "/Users/test/\u{e4}\u{b8}\u{ad}\u{e6}\u{96}\u{87}.txt" (garbled)
```

## Proposed Solutions

### Option A: Use `urlencoding` Crate (Recommended)
**Pros:** Correct UTF-8 handling, well-tested, 1 line of code
**Cons:** Adds dependency
**Effort:** Low
**Risk:** Very Low

```toml
# Cargo.toml
urlencoding = "2.1"
```

```rust
fn parse_file_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    let decoded = urlencoding::decode(path).ok()?;
    Some(PathBuf::from(decoded.into_owned()))
}
```

### Option B: Use `percent-encoding` Crate
**Pros:** More control, widely used
**Cons:** Slightly more verbose
**Effort:** Low
**Risk:** Very Low

```rust
use percent_encoding::percent_decode_str;
let decoded = percent_decode_str(path).decode_utf8().ok()?;
```

### Option C: Fix Manual Implementation
**Pros:** No new dependencies
**Cons:** More code, easy to get wrong
**Effort:** Medium
**Risk:** Medium

Collect bytes into a Vec<u8> first, then convert to String with `String::from_utf8`.

## Recommended Action

_To be filled during triage_

## Technical Details

**Affected Files:**
- `src/connectors/cursor.rs` (lines 248-274)

**Components:**
- `parse_file_uri()` function

## Acceptance Criteria

- [ ] Paths with Chinese/Japanese/Korean characters decode correctly
- [ ] Paths with emojis decode correctly
- [ ] All existing tests pass
- [ ] Add test for multi-byte UTF-8 decoding

## Work Log

| Date | Action | Outcome/Learning |
|------|--------|------------------|
| 2026-01-02 | Identified by Security Sentinel | UTF-8 handling bug confirmed |

## Resources

- PR #26: https://github.com/Dicklesworthstone/coding_agent_session_search/pull/26
- urlencoding crate: https://docs.rs/urlencoding
