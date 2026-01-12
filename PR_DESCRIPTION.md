# Add Custom rsync Path Support for Remote Sources

## Problem

When syncing from macOS remote machines, cass fails if the remote machine has an outdated system rsync (e.g., macOS ships with rsync 2.6.9 from 2006). Modern rsync flags used by cass (like `--protect-args`) don't exist in old versions, causing sync failures.

Even when users install modern rsync via Homebrew (`/opt/homebrew/bin/rsync`), cass cannot use it because there's no way to specify a custom rsync path on the remote machine.

Additionally, remote mirror directories were not being properly indexed due to issues in `build_scan_roots()`.

## Solution

This PR implements three fixes:

### 1. Add `rsync_path` Configuration Field

**File:** `src/sources/config.rs`

Add optional field to `SourceDefinition`:

```rust
/// Custom rsync path on remote machine.
/// If specified, uses --rsync-path flag to use a custom rsync binary.
/// Example: "/opt/homebrew/bin/rsync"
#[serde(default)]
pub rsync_path: Option<String>,
```

### 2. Implement --rsync-path Support in Sync

**File:** `src/sources/sync.rs`

**2a. Update function signature:**
```rust
fn sync_path_rsync(
    &self,
    host: &str,
    remote_path: &str,
    dest_dir: &Path,
    remote_home: Option<&str>,
    rsync_path: Option<&str>,  // Add this parameter
) -> PathSyncResult {
```

**2b. Add documentation:**
```rust
/// The `rsync_path` parameter specifies a custom rsync binary path on the remote machine.
```

**2c. Insert rsync-path flag before -e option:**
```rust
// After "--timeout" argument, before "-e" argument:
// Add custom rsync path if specified
if let Some(path) = rsync_path {
    cmd.arg("--rsync-path").arg(path);
}
```

**2d. Update caller to pass rsync_path:**

In `sync_source()` method, change:
```rust
self.sync_path_rsync(host, remote_path, &mirror_dir, remote_home.as_deref())
```
to:
```rust
self.sync_path_rsync(host, remote_path, &mirror_dir, remote_home.as_deref(), source.rsync_path.as_deref())
```

### 3. Fix Remote Mirror Indexing

**File:** `src/indexer/mod.rs`

**3a. Remove early return (line ~792):**

Change:
```rust
}
return roots;  // Remove this line
}
```
to:
```rust
}
// Don't return early - fall through to scan whole mirror directories
}
```

**3b. Comment out path-specific matching in fallback (lines ~842-882):**

The fallback code attempts to match individual paths but has tilde expansion issues.
Comment out the entire `if let Some(paths) = source.config_json...` block and add:

```rust
// Skip path-specific matching in fallback - it has the same tilde expansion issue.
// Instead, always scan the entire mirror directory below.
```

**3c. Scan mirror subdirectories individually (line ~890+):**

Replace:
```rust
if mirror_path.exists() {
    let origin = Origin {
        source_id: source.id.clone(),
        kind: source.kind,
        host: source.host_label.clone(),
    };
    let mut scan_root = ScanRoot::remote(mirror_path, origin, platform);
    scan_root.workspace_rewrites = workspace_rewrites;
    roots.push(scan_root);
}
```

With:
```rust
if mirror_path.exists() {
    // Scan each subdirectory in the mirror as a separate root
    // This handles the case where rsync syncs full paths like
    // "Users_username_.claude_projects"
    if let Ok(entries) = std::fs::read_dir(&mirror_path) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let origin = Origin {
                    source_id: source.id.clone(),
                    kind: source.kind,
                    host: source.host_label.clone(),
                };
                let mut scan_root = ScanRoot::remote(entry.path(), origin.clone(), platform);
                scan_root.workspace_rewrites = workspace_rewrites.clone();

                tracing::info!(
                    source_id = %origin.source_id,
                    scan_path = %entry.path().display(),
                    "added remote scan root"
                );

                roots.push(scan_root);
            }
        }
    }
}
```

**3d. Add logging for remote conversations (line ~209+):**

After `Ok(mut remote_convs) => {`, add:
```rust
tracing::info!(
    connector = name,
    source_id = %root.origin.source_id,
    count = remote_convs.len(),
    "scanned remote conversations"
);
```

## Configuration Example

`~/.config/cass/sources.toml`:
```toml
[[sources]]
name = "my-mac"
type = "ssh"
host = "user@hostname"
paths = [
    "~/.claude/projects",
    "~/.codex/sessions",
]
sync_schedule = "hourly"
platform = "macos"
rsync_path = "/opt/homebrew/bin/rsync"  # NEW: Custom rsync path
```

## Testing

1. Install modern rsync on remote macOS machine:
   ```bash
   brew install rsync
   ```

2. Configure source with `rsync_path`:
   ```toml
   rsync_path = "/opt/homebrew/bin/rsync"
   ```

3. Test sync:
   ```bash
   cass sources doctor  # Should show connectivity
   cass sources sync    # Should succeed
   cass index --full    # Should index remote sessions
   ```

4. Verify:
   ```bash
   cass stats  # Should show conversations from remote source
   cass search "query" --source my-mac  # Should work
   ```

## Backwards Compatibility

- The `rsync_path` field is optional, defaults to None
- Existing configurations without `rsync_path` continue to work unchanged
- When `rsync_path` is None, rsync behavior is identical to before

## Notes for Maintainers

Since you prefer to re-create PRs rather than merge them directly, here's the implementation checklist:

- [ ] Add `rsync_path: Option<String>` field to `SourceDefinition` in `src/sources/config.rs`
- [ ] Add `rsync_path` parameter to `sync_path_rsync()` in `src/sources/sync.rs`
- [ ] Add `--rsync-path` flag to rsync command when `rsync_path.is_some()`
- [ ] Update `sync_source()` to pass `source.rsync_path.as_deref()`
- [ ] Remove early `return roots;` in `build_scan_roots()` in `src/indexer/mod.rs`
- [ ] Comment out path-specific matching in fallback section
- [ ] Add subdirectory iteration for mirror scanning
- [ ] Add logging for remote conversation scanning

All changes maintain backward compatibility and add no new dependencies.
