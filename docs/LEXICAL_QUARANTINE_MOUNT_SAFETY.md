# Linux mount boundaries during lexical quarantine retention

A same-filesystem bind mount can expose unrelated data with the same device
number and ordinary link counts. A recursive removal that refuses symlinks is
not, by itself, a refusal to traverse mount points.

The lexical quarantine inventory now checks `/proc/thread-self/mountinfo`
before traversing each bundle on Linux. A mount at the bundle itself, at a
subdirectory, or at a regular file makes that observation incomplete. The
existing incomplete-inventory rule then permits no reclamation. Mounts above
the bundle, and sibling mounts, are not disqualifying: an index may live on its
own filesystem. Mount points are compared by path components, not string prefix.

Both automatic startup retention and approved offline cleanup re-read this
boundary after target/survivor checks, before the rename, and again immediately
before planned entry removal. A refusal after the rename leaves the recognized
`.lexical-rebuild-reclaim-*/quarantine` reservation intact. Inspect again and
obtain a new cleanup preview after resolving the mount; do not replay an old
approval after a partially executed cleanup.

The parser preserves non-UTF-8 path bytes and decodes the kernel's space, tab,
newline and backslash escapes. Unknown optional mountinfo fields are ignored.
Missing, unreadable, malformed, empty, truncated or over-budget mount evidence
is a refusal, never evidence that no mounts exist. No mount point is opened by
the parser, and it neither mounts nor unmounts anything.

Each snapshot is limited to 1 MiB, and all snapshots in one inventory/recheck
budget share 16 MiB of read work, plus at most one overflow-detection byte. The
existing cooperative deadline also applies. Individual filesystem calls remain
synchronous; this is not a hard wall-clock or total-process-memory bound.
A Linux installation without readable proc mountinfo cannot authorize this
cleanup. Existing non-Linux checks are unchanged; this is not a new cross-platform
mount-point guarantee.

Mount namespace and ancestor directories remain operator-controlled. These
checks detect observed mount boundaries and ordinary changes between stages;
they do not atomically exclude a privileged process changing mounts during a
filesystem call. Do not change mounts while inventory or cleanup is running.

Regression tests cover same-device root/directory/file mounts, sibling and
ancestor boundaries, escaped/non-UTF-8 paths, malformed tables, read and work
budgets, pre-walk refusal, real read-only Linux mountinfo inspection, refusals
before/after the rename, interrupted cleanup retry, and preserved newest
failure evidence. Fault-injected tests do not create actual bind mounts.

## Bounded, descriptor-relative removal on Linux

The Linux deletion path no longer calls `remove_dir_all`. During the locked
target recheck it records the exact observed relative names, types, inode/device
identities and regular-file metadata. This is the same observation used to
compare the inventory fingerprint: there is no second policy, persisted path
list or additional walk. Only one target plan is retained at a time, bounded
by the existing tree-entry, depth and path-byte limits. Its path/metadata storage
is additional to the inventory walk's temporary hash input, not an RSS bound.

After the existing reservation rename and mount checks, removal opens the
managed parent and the target through identity-checked directory descriptors.
It uses the current thread's `/proc/thread-self/fd/<fd>` magic links as a safe
standard-library bridge to those open directories, verifying each link's inode
and device against the owned descriptor. It walks one component at a time with
`O_DIRECTORY | O_NOFOLLOW`, checks named entries with `symlink_metadata`, and
removes only captured names with nonrecursive `remove_file`/`remove_dir` calls.
Regular files must still match identity, size,
modification/change timestamps and the single-link requirement. Directories
are removed only when empty; newly added children are never recursively swept.
A symlink, replacement, changed file, unexpected child, I/O failure or budget
exhaustion stops further removal and leaves recognizable interrupted evidence.
No payload file is opened. The descriptor bridge forbids unsafe code and adds
no dependency. Usable current-thread proc fd links are required in addition to
mountinfo; there is no fallback to recursively deleting an ordinary pathname.

All unlink/rmdir attempts in a sweep share a 65,536-entry limit and the existing
cooperative recheck deadline. The deadline is checked between operations and
while descending parents, but cannot preempt one blocked filesystem call.
Partial work is not rolled back or counted as a fully removed bundle. A retry
requires a fresh inventory/approval and still honors the six-hour hold-down
on recently changed interrupted artifacts. Newest/recent evidence, survivor
checks and index-run locking are unchanged. Other platforms retain the existing
standard-library removal implementation.

Descriptor-relative traversal avoids symlink redirection; it does not make
metadata-check plus unlink atomic. A non-cooperating writer can still replace
a name between those calls or move an opened directory. Keep the managed roots
operator-controlled and do not modify evidence while cleanup holds the lock.
The diagnostic/doctor call-site integration is separate from this deletion path.

The additional Rust regressions cover late unknown files, symlink/special-file
and hard-link replacements, changed root/parent identities, byte-exact non-UTF-8
names, spent work/deadline budgets, partial removal and fresh retry, and repeated
production sweeps preserving newest evidence. These Rust tests still require
native execution; independent Linux syscall probes are not Rust qualification.

Native qualification commands (not claimed executed successfully here):

```sh
cargo test --locked --lib lexical_quarantine
cargo test --locked --lib staging_reclaim
cargo test --locked --test cli_lexical_quarantine
cargo test --locked --test cli_lexical_quarantine_cleanup
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```
