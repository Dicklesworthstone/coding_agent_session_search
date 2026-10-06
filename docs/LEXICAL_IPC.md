# Guarded lexical IPC for fresh clients

This is an **explicit process-backed fast path**, not a change to standalone
Quill admission. A foreground owner retains one strictly admitted `cass serve`
reader. Independent `cass-query` processes communicate with that owner over a
private local socket, so an unchanged index does not need a new native reader
for each client. There is no second search engine, response cache, persisted
verification receipt, dependency override, or silent daemon startup.

Ordinary `cass search`, writer admission, indexing, recovery, and maintenance
remain unchanged. Without an explicitly managed owner, they retain their
existing full-verification behavior. This does not by itself close issue #501's
self-contained fresh-process index-open case.

## Run it

Linux, procfs, and Python 3.10 or newer are required for the owner. The native
client is an automatically discovered Cargo binary:

```sh
cargo build --locked --release --bin cass --bin cass-query
install -d -m 0700 "$HOME/.cache/cass-lexical"
python3 -S scripts/cass_lexical_ipc.py serve \
  --cass "$PWD/target/release/cass" \
  --index /absolute/path/to/published-lexical-index \
  --socket "$HOME/.cache/cass-lexical/owner.sock"
```

The owner stays in the foreground. Manage it in another terminal or with your
existing process supervisor. It does not load an index until a valid search,
exits after 300 idle seconds by default, and never creates the socket parent
or writes into the index. Use `--idle-seconds` and `--timeout` to set bounded
operating limits. The native CASS child inherits the owner's stderr.

From another process:

```sh
target/release/cass-query \
  --socket "$HOME/.cache/cass-lexical/owner.sock" \
  --limit 10 'performance'
```

The Python frontend is also available without building the extra client:

```sh
python3 -S scripts/cass_lexical_ipc.py search 'performance' \
  --socket "$HOME/.cache/cass-lexical/owner.sock"
```

These clients return the `cass serve` JSON envelope and bounded index previews,
not the ordinary `cass search` CLI's formatting or canonical message bodies.
`--filters` accepts a JSON object containing the service's lexical filters;
`--limit` and `--offset` use its existing bounds. There is no semantic search,
canonical database access, automatic archive selection, or maintenance command.
The fixed index is selected by the owner, never by an individual socket request.

## Full-verification escape hatches

```sh
target/release/cass-query \
  --socket "$HOME/.cache/cass-lexical/owner.sock" \
  --full-verify 'performance'
```

`--full-verify` discards and reaps the retained native process before starting a
new `cass serve` reader. It is not a flag that merely changes output labeling.
The native frontend rejects a response that says a retained reader satisfied
this request. Later requests can reuse the newly admitted reader.

To bypass the owner and socket completely, use a fixed index directly:

```sh
python3 -S scripts/cass_lexical_ipc.py search 'performance' \
  --cass "$PWD/target/release/cass" \
  --index /absolute/path/to/published-lexical-index
```

Direct `--index` always starts a new fully verified native reader. All ordinary
CASS maintenance/indexing commands remain outside this transport and policy.

## Integrity boundary

The owner captures index identities **before** native admission. It retains
open handles to every admitted file and directory, rejecting symlinks and
special files. Before and after each search it checks descriptor and pathname
identity, device/inode, type, ownership, link count, length, nanosecond mtime and
ctime, and directory membership. Retained handles prevent inode recycling
from being confused with the admitted file. This includes manifests and
sidecars, not just segment payloads.

An observed change before a query destroys the old native reader and requires
fresh strict admission. Corruption is not hidden by falling back to the old
snapshot. An observed change during a query discards that query's result,
destroys the reader, and reports `index_changed`. A later request can retry
after publication settles. Replacement, truncation, same-length writes, and
writes followed by mtime restoration are covered by implementation regressions.

**Metadata equality is not byte-integrity proof.** This path relies on Quill's
immutable-publication discipline, like the underlying retained-reader service.
It does not certify an immutable generation, detect every media fault or shared
writable-mapping mutation, defend against an attacker preserving change
metadata, or provide a cross-archive freshness guarantee. Use coherent local
filesystems; network/cached filesystem behavior is not qualified. A change after
the final guard check is a normal concurrency boundary. Use full verification
or ordinary standalone admission when these retained-reader assumptions are
not appropriate.

The response adds `admission.mode` (`strict_full` or `retained_guarded`),
`owner_epoch`, `full_verify_requested`, `file_identity_checked: true`,
`persistent_proof: false`, and `immutable_generation_certified: false`.
`owner_epoch` is a local successful-admission counter, not a database generation.
The original native `snapshot` metadata is preserved.

## Why this design

A size/mtime receipt is inadequate for same-length writes and restored mtime.
Adding inode and ctime improves ordinary mutation detection but still does not
make a persistent receipt equivalent to rehashing bytes. Section-on-use checks
also change the existing admission contract by leaving unvisited bytes unchecked.
A stronger filesystem-backed integrity capability needs integration inside
Quill's authenticated-admission boundary; a CASS wrapper cannot simply skip
hashing performed inside the pinned published dependency.

This implementation instead keeps native admission unchanged and reuses its
live owner. It trades a deliberately managed process and resident reader memory
for avoiding repeated admission. No private Quill fork or unchecked snapshot
constructor is introduced. It is a useful deployment alternative, not evidence
that disk metadata can authenticate a fresh standalone reader.

## Transport and lifecycle bounds

Both peers authenticate the effective user ID with Linux `SO_PEERCRED`.
The socket must reside in an owned private directory; clients and the owner
walk without symlinks and retain its descriptor through bind/connect. Socket
permissions are 0600. The same user and root remain trusted principals: this
is not a sandbox against a malicious same-user executable.

A retained advisory lock prevents concurrent owners at the same endpoint.
The lock file's **contents have no verification meaning**. Corrupting them
cannot admit an index. Existing endpoints are never automatically deleted;
after an unclean termination, remove a stale socket only after confirming its
owner is gone. Normal SIGTERM, Ctrl-C, and idle exit clean up the owned socket
and child. The empty lock file may remain.

One connection carries one request. Requests are limited to 64 KiB, responses
to 1 MiB, and lexical queries/pages to the native service's limits. Unknown
operations and fields are refused. Duplicate JSON keys, malformed native
responses, mismatched IDs, and oversized frames fail closed. Native requests
receive owner-local unique IDs, independent of repeated client IDs.

The owner serializes requests and caps the socket backlog at eight. Guards
allow at most 4,096 file/directory handles and depth 16; oversized or unsafe
layouts fail rather than silently dropping coverage. Ensure the supervisor's
file-descriptor limit can accommodate the index plus the native reader.

The default request budget is 30 seconds. The native frontend has a whole-
operation watchdog, including connect and output, that terminates it with exit
124; it does not emit a timeout JSON frame. Host supervision remains necessary
for the foreground owner, especially for uninterruptible filesystem operations.
The owner's native child retains CASS's own request watchdog. No stalled native
request is left running as a successful background continuation.

## Validation status and commands

```sh
python3 -S -m unittest discover -s scripts -p 'test_cass_lexical_ipc.py' -v
cargo test --locked --bin cass-query
```

The 20 Python tests run the real guard and socket implementation against a
fake strict worker. They exercise fresh-process reuse, forced verification,
valid publication, adversarial mutations, corrupted protocol/lock state,
unsafe paths, timeout/child cleanup, duplicate owners, and descriptor cleanup.
They are not Quill tests or CASS performance measurements. Native client tests
must be run separately; adding their definitions is not execution evidence.
