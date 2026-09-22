# Persistent lexical search over standard I/O

`cass serve` lets a coding agent or local coordinator keep one lexical reader
open across repeated searches instead of paying the full index-open cost for
one `cass search` process per query. It uses the production `SearchClient`
parser, scorer, source filtering, and result reducer. It does not implement a
second search engine.

```sh
cass serve --stdio --data-dir /path/to/cass-data
```

Alternatively, select a published lexical index directory directly:

```sh
cass serve --stdio --index /path/to/published-lexical-index
```

Exactly one explicit location and `--stdio` are required. `--data-dir` resolves
the index path using the compiled CASS schema version without creating any
folders. It does **not** open the canonical database. There is no implicit
selection of another user archive. Use `cass serve --help` for this command's
independent parser; it dispatches before ordinary CLI readiness/maintenance
setup, like the logical-archive command. Existing root introspection does not
yet enumerate this independent command.

## Lifecycle and snapshot contract

Starting the process, requesting status, or submitting an invalid query does
not load an index. The first valid search lazily opens it. Subsequent searches
reuse that same admitted lexical reader; query-prefix response caching and
background prewarming are disabled. Newly published indexing work is not
silently mixed into an existing session.

`reload` releases the old reader **before** opening its replacement, so this
operation never intentionally retains two multi-gigabyte reader generations.
If opening the replacement fails, the session is unloaded. It does not silently
fall back to the old reader. A later reload or valid search may retry the same
fixed path. Shutdown and end-of-input release the reader and terminate; there
are no detached children or network listeners.

`reader_epoch` is a monotonically increasing **session-local** successful-open
counter, not a canonical database generation, checksum, or freshness proof.
Responses explicitly report `snapshot_policy: "pinned_until_reload"` and
`freshness: "not_checked"`. Status means only whether this process has a loaded
reader, not whether the archive is complete, current, or healthy.

Search returns **index previews**, not canonical message bodies. It
never supplies a database path to `SearchClient`, loads semantic models,
rebuilds indexes, initiates automatic refresh, or downloads assets. Keep
indexing in a separately managed maintenance process. To validate or inspect
a hit against the canonical archive, retain its `source_id`,
`conversation_id`, and `message_index` for the ordinary CASS follow-up commands.
`message_index` is one-based canonical message addressing, **not a raw file
line number**. Identity fields are never shortened to fit an output budget.
For complete bounded bodies in the same service, opt in to `view` using the
fixed `--db` configuration described below. No archive is inferred from `--data-dir`.

## Wire protocol, version 1

Send one UTF-8 JSON object per line; each request gets one JSON response line.
Flush after sending. There are no unsolicited stdout messages. Requests are
processed sequentially, so backpressure does not create an in-process request
queue. Without `--mcp`, this is a CASS JSON-lines protocol, not JSON-RPC.
The MCP adapter described below uses the same reader and bounds.

Every request requires an unsigned 64-bit `id`. Five operations are always available:

```json
{"op":"status","id":1}
{"op":"search","id":2,"query":"performance","limit":10,"offset":0,"filters":{"agents":["codex"],"workspaces":["/my/project"],"source_id":"work-laptop"}}
{"op":"reload","id":3}
{"op":"unload","id":4}
{"op":"shutdown","id":5}
```

Search defaults to `limit: 10`, `offset: 0`, and no filters. `filters` may contain
`agents`, `workspaces`, `source_id`, `created_from`, and `created_to`; timestamps
are integer Unix milliseconds and bounds are inclusive according to the
existing lexical engine. `source_id` is always one exact ID: strings such as
`remote` and `all` are **not** special groups here. To search every source,
omit that field. Empty agent/workspace lists mean unrestricted.

Unsupported fields and operations are errors, never silently ignored. There
is no `mode`, semantic fallback, session-path filter, arbitrary per-request
index path, full-content search, or maintenance operation. Canonical `view` is
separately enabled by `--db`. In particular,
post-filter routes that can expand to corpus-sized candidate windows are not
exposed as bounded service filters.

A response has `schema_version`, `id`, `ok`, and either `result` or `error`.
Malformed requests use `id: null`. A failed request does not terminate the
session except on transport failure, an oversized frame, or a process deadline.
Ordinary request errors retain their
cause in `error.message`; callers should branch on the stable `error.kind`.

Search results contain `hits`, `count`, `limit`, `offset`, `reader_reused`,
`setup_ms`, `search_ms`, `preview_only`, and the session `snapshot` metadata.
Hits preserve source, conversation, one-based message, agent, workspace,
timestamp, origin and score fields. Titles are limited to 256 characters and
snippets to 800 characters, on Unicode character boundaries. No `content`
field is returned.

Pagination probes for one extra hit. `has_more: true` means an extra hit was
actually observed. When another same-sized page is within the service budget,
`next_offset` supplies its offset. With no observed extra hit, both fields are
`null`: bounded candidate selection does not prove that the complete corpus is
exhausted. There is no exact-total-count claim.

When another same-sized page would exceed the service's page window,
`next_offset` is null and `page_window_exhausted` is true, even if an extra hit
was observed. Narrow the query or filters rather than following an invalid
continuation offset.

## Bounds and limitations

Request frames are limited to 64 KiB excluding the newline; responses are
limited to 1 MiB including JSON escaping and the newline. An oversized request
gets one error and closes the session without draining an unlimited suffix.
An oversized response is replaced with a small error before any of its bytes
are published; it is not truncated into an apparently successful result.

Queries must be nonempty and at most 4,096 UTF-8 bytes. Limits must be 1–100,
and `offset + limit + 1` must not exceed 1,024. Each agent/workspace filter has
at most 32 values. Individual filter and returned identity strings are limited
to 4,096 UTF-8 bytes. Invalid budgets are refused before the reader is loaded.

These are transport and candidate-window bounds, **not a total-RSS bound**.
The first index admission can still be expensive. Each independently started worker still owns
its own reader: reuse one process rather than spawning one for each query.
Semantic/HNSW serving is not implemented by this lexical endpoint. Optional
shared reader admission is described below; it bounds owners, not total RSS.

### Enforced request deadlines

Both transports default to a **30,000-ms whole-request deadline**. Set
`--request-timeout-ms 5000` for a five-second budget, or explicitly allow a
longer cold open. Accepted values are 1–300,000 ms; zero cannot disable the
guard. Status reports the configured deadline and termination exit code.

The deadline starts when the worker first observes any byte of a new frame and
covers the rest of input framing, decoding, native index admission/query or
canonical lookup, result encoding, and output flushing. A partial line and a
client that stops reading responses are therefore bounded too. Waiting for
the first byte of the next request is idle time and is deliberately excluded.
Completing a request joins its watchdog before another request can be admitted;
an old timer cannot terminate an unrelated later query. Reader teardown on EOF
or transport failure is guarded separately with the same configured duration.

**Expiry terminates the entire read-only worker with exit code 124**, rather
than returning a soft timeout while native work continues in a detached thread.
There is no timeout JSON/tool-error response: producing one could itself block
on the stalled output pipe. Discard an incomplete final line, observe process
exit, and restart/reinitialize before retrying. Earlier complete responses stay
valid. The new process starts a new session-local reader epoch; do not equate
epochs across processes. Failure of the watchdog itself exits with code 125.

Termination does not wait for Rust destructors or flush evidence buffers.
Use this mechanism only on the service's read-only paths, never for indexing
or archive publication. It does not impose a memory limit, alter the native
engine's cancellation API, or guarantee real-time OS scheduling under system
suspension/starvation. Hosts should still supervise the worker process and
enforce their own end-to-end deadlines, including startup and idle lifetime.

## Reusing one process from Python

```python
import json
import subprocess

with subprocess.Popen(
    ["cass", "serve", "--stdio", "--data-dir", "/path/to/cass-data"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    text=True,
    encoding="utf-8",
) as worker:
    def exchange(request):
        worker.stdin.write(json.dumps(request) + "\n")
        worker.stdin.flush()
        # Text-mode limit is a defensive client bound; the server enforces bytes.
        line = worker.stdout.readline(1024 * 1024 + 1)
        if not line or not line.endswith("\n"):
            raise RuntimeError("search worker closed or returned an invalid frame")
        response = json.loads(line)
        if response["id"] != request["id"] or not response["ok"]:
            raise RuntimeError(response)
        return response["result"]

    try:
        first = exchange({"op": "search", "id": 1, "query": "performance"})
        second = exchange({"op": "search", "id": 2, "query": "profiling"})
        assert second["reader_reused"]
        # Reload deliberately when the caller chooses to adopt a new publication.
        exchange({"op": "reload", "id": 3})
        exchange({"op": "shutdown", "id": 4})
    finally:
        worker.stdin.close()
        try:
            worker.wait(timeout=30)
        except subprocess.TimeoutExpired:
            worker.kill()
            worker.wait()
```

The example waits synchronously for replies and will observe EOF if the worker
times out. Applications should additionally supervise startup and the exchange
against their own deadline; the worker timeout is a process-termination policy,
not resumable per-query cancellation.

Native regressions are in `tests/search_service.rs`, including the complete
production module's tests: real Quill reader reuse, source-scoped identities,
publication isolation, explicit/failed reload, no archive writes, frame bounds,
invalid-request recovery, pagination uncertainty, and actual binary dispatch.
Run `cargo test --locked --test search_service -- --test-threads=1` through the repository's normal
validation environment. Test definitions alone are not execution evidence.

## MCP integration

An MCP host can launch the same read-only worker directly:

```json
{
  "mcpServers": {
    "cass": {
      "command": "/absolute/path/to/cass",
      "args": ["serve", "--stdio", "--mcp", "--data-dir", "/path/to/cass-data"]
    }
  }
}
```

The adapter implements the initialization-based MCP revisions **2025-11-25**
and **2025-06-18**. Send `initialize`, then `notifications/initialized`, before
calling `tools/list` or `tools/call`. Unsupported handshake versions are
counter-offered 2025-11-25. The newer stateless 2026-07-28 protocol is not
advertised: `server/discover` returns Method Not Found, allowing dual-era MCP
clients to fall back to the supported handshake. A modern-only client cannot
use this adapter. Protocol behavior follows the versioned MCP lifecycle and
stdio specifications, not an assumption that every version uses initialization.

The catalog exposes `cass_search`, `cass_status`, `cass_unload`, and `cass_reload`.
With an explicit startup `--db`, it also exposes the read-only `cass_view` tool.
`cass_search` arguments are the JSON-lines search fields **without** `op` or
`id`; status, unload and reload accept an empty object. The JSON-RPC ID is preserved
exactly, including string and signed-integer IDs. There is no arbitrary file
reader, SQL tool, shell command, indexing tool, or semantic-mode substitution.
Tool results include both `structuredContent` and its JSON representation in
a text content block. Retrieval and budget failures are visible to the model
as `isError: true`; invalid RPC methods/envelopes remain protocol errors.

Initialization, discovery, and status do not load an index. Search and reload
share the exact same session handler as the CASS protocol; reader reuse,
release-before-reload, source coordinates, and unchecked-freshness metadata
are unchanged. All callers using one worker share its reader. An explicit
`cass_reload` affects that worker's subsequent queries, not another worker.
At most 120 valid tool invocations are admitted per minute per process; excess
calls get a nonblocking `rate_limited` tool error instead of a queued operation.

Notifications receive no response and cannot run a tool without a request ID.
There is no MCP shutdown RPC: close stdin, then terminate the process if its
external deadline expires. Requests are serial, so cancellation notifications
received after native work completes are ignored. This adapter does **not**
provide resumable per-call native cancellation, a global memory governor, HTTP, or
authentication over a network. Configure the host to approve access to the
selected history archive and treat returned session text as untrusted data.

MCP lifecycle, malformed-frame, ID, quota, real-reader/reload, and binary
handshake regressions run in the same `search_service` integration target.
Specifications: https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle
and https://modelcontextprotocol.io/specification/2025-11-25/server/tools.
The era-fallback contract is documented at
https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning.


## Canonical evidence follow-up (explicit opt-in)

Enable complete message reads from one fixed archive, independently of the
retained lexical reader:

```sh
cass serve --stdio --mcp --data-dir /path/to/cass-data --db /path/to/cass-data/agent_search.db
```

The `--db` option grants access only to that configured canonical archive.
Starting the process, initialization, discovery, status and lexical searches
still do not open it. No database path is accepted inside a request. Omitting
`--db` keeps the original index-only capability: `cass_view` is absent from the
MCP catalog and cannot be invoked by guessing its name. JSON-lines `view` then
returns `canonical_access_disabled`.

Without `--mcp`, send:

```json
{"op":"view","id":5,"source_path":"/history/session.jsonl","source_id":"local","conversation_id":42,"message_index":13,"context":1}
```

With MCP, call `cass_view` using the same arguments without `op` or `id`.
Copy **all four coordinates** from one search hit. They are not interchangeable:
`source_path` and `source_id` must match the stored conversation exactly,
`conversation_id` must be positive, and `message_index` is the one-based
canonical ordinal. No source alias, path normalization, raw-file fallback or
neighbour substitution is attempted. The named source file need not exist;
its path is compared as identity data and is never opened.

`context` defaults to zero and accepts 0–20 actual messages on each side.
Sparse indices are preserved: for indices 8, 13 and 100, a view of 13 with
context 1 returns those three messages, not the arithmetic range 12–14.
The target must exist exactly. Identity checks, metadata selection and body
hydration share one explicit read transaction through the production strict
FrankenSQLite reader; requested malformed content or ambiguous coordinates
fail the entire operation. Database files that are symlinks are rejected at
admission. This preliminary path check is not an atomic filesystem path pin.

A view returns root source/conversation coordinates, `messages` with exact
`message_id`, `message_index`, `role`, complete `content`, and `is_target`, plus
`content_bytes` and `more_before` / `more_after`. The latter fields are null
when context is zero because no neighbour probe occurs. There is no
whole-conversation count or archive-integrity claim.

The complete window is limited to **64 KiB of UTF-8 message-body bytes**.
Byte-length guards run inside SQL before oversized content is transferred to
the service, and count embedded NULs and multibyte characters. Content is
never shortened to manufacture success. Oversized windows return
`canonical_payload_too_large`; reduce context or use the explicit CLI follow-up
for a larger individual message. The existing 1 MiB encoded response limit
still applies, including both MCP result representations and JSON escaping.
Other typed errors distinguish missing targets, identity mismatches, invalid
coordinates and elapsed cooperative budgets; storage failures retain context.

Each view opens a **new canonical read snapshot**, releases its transaction and
closes its reader before the next request. It does not replace or relabel the
retained lexical reader. Results state
`snapshot_policy: "one_archive_read_transaction_per_view"`,
`matches_lexical_snapshot: null`, and `lexical_freshness: "not_checked"`.
A successful view is not proof that the earlier search preview is current.
Status exposes capability and attempted/completed read counters without
probing either storage path.

The 3-second lookup budget is cooperative: checked between admission and SQL
operations, with no partial result on expiry. It does **not** interrupt an
individual engine call, bound engine-internal allocation, or replace the
host's process-level deadline. No maintenance, recovery write, model loading,
raw transcript read, arbitrary SQL or new filesystem authority is provided.
The enclosing service request deadline also covers this lookup, including an
individual native SQL call, by terminating the worker if necessary.

## Shared reader admission (explicit opt-in)

Use `--admission-dir /trusted/local/cass-reader-pool --admission-slots 1`
on `cass serve --stdio` or `cass serve --stdio --mcp`. Every cooperating
worker must use the same local directory and slot count, including workers
serving different indexes. Counts are 1–64, default one. Without the directory
option, shared admission is disabled. This is not a query relay: reuse an
admitted worker for repeated queries.

A worker acquires one kernel-backed lease before opening its lexical reader
and retains it through reader destruction. A canonical view uses the existing
lease or a temporary one through archive close. Busy admission returns
`admission_busy` before index-open or canonical-read counters advance. Failed
opens release their temporary lease. Status and invalid requests do not
initialize or inspect the pool.

`unload` / MCP `cass_unload` releases the reader and then its lease without
closing the connection. The next query reacquires and reopens. Reload also
releases first; another process can win the slot, leaving a failed reloader
unloaded. Kernel locks release on process termination; PIDs and timestamps
are not ownership certificates. Existing deadlines and source checks remain.

This explicit option writes only a versioned policy and fixed slot lock files.
Choose a private trusted local directory outside indexes and source trees.
Never delete, replace or move pool files or the directory while workers may
be alive: this splits the lock namespace. Different slot counts and partial
policies fail closed with `admission_policy_mismatch`; there is no automatic
repair or stale-file deletion. Symlink preflight is not an atomic sandbox
against a hostile directory owner; network-filesystem locking is unqualified.

The limit covers participating reader owners, not total machine memory. Other
programs, workers without the option, and allocator-retained pages after
unload are outside the count. Status reports the scope, slots and held lease.
