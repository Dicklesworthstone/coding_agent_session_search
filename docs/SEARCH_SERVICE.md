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

This service returns **index previews**, not canonical message bodies. It
never supplies a database path to `SearchClient`, loads semantic models,
rebuilds indexes, initiates automatic refresh, or downloads assets. Keep
indexing in a separately managed maintenance process. To validate or inspect
a hit against the canonical archive, retain its `source_id`,
`conversation_id`, and `message_index` for the ordinary CASS follow-up commands.
`message_index` is one-based canonical message addressing, **not a raw file
line number**. Identity fields are never shortened to fit an output budget.

## Wire protocol, version 1

Send one UTF-8 JSON object per line; each request gets one JSON response line.
Flush after sending. There are no unsolicited stdout messages. Requests are
processed sequentially, so backpressure does not create an in-process request
queue. This is a CASS JSON-lines protocol, **not MCP or JSON-RPC**.

Every request requires an unsigned 64-bit `id`. Four operations are supported:

```json
{"op":"status","id":1}
{"op":"search","id":2,"query":"performance","limit":10,"offset":0,"filters":{"agents":["codex"],"workspaces":["/my/project"],"source_id":"work-laptop"}}
{"op":"reload","id":3}
{"op":"shutdown","id":4}
```

Search defaults to `limit: 10`, `offset: 0`, and no filters. `filters` may contain
`agents`, `workspaces`, `source_id`, `created_from`, and `created_to`; timestamps
are integer Unix milliseconds and bounds are inclusive according to the
existing lexical engine. `source_id` is always one exact ID: strings such as
`remote` and `all` are **not** special groups here. To search every source,
omit that field. Empty agent/workspace lists mean unrestricted.

Unsupported fields and operations are errors, never silently ignored. There
is no `mode`, semantic fallback, session-path filter, arbitrary per-request
index path, full-content request, or maintenance operation. In particular,
post-filter routes that can expand to corpus-sized candidate windows are not
exposed as bounded service filters.

A response has `schema_version`, `id`, `ok`, and either `result` or `error`.
Malformed requests use `id: null`. A failed request does not terminate the
session except when its frame exceeds the byte limit. Errors retain their
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

These are transport and candidate-window bounds, **not a total-RSS bound or a
wall-clock deadline**. The first index admission can still be expensive, and
an individual native query is not forcibly preempted by the service. The
caller owns process lifetime and may terminate this read-only worker when its
external deadline is exceeded. Each independently started worker still owns
its own reader: reuse one process rather than spawning one for each query.
Semantic/HNSW serving and cross-process admission are not implemented by this
lexical endpoint.

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

The example waits synchronously for replies; applications requiring a hard
query deadline must additionally supervise the exchange itself. It does not
turn the native engine into a cancellable operation.

Native regressions are in `tests/search_service.rs`, including the complete
production module's tests: real Quill reader reuse, source-scoped identities,
publication isolation, explicit/failed reload, no archive writes, frame bounds,
invalid-request recovery, pagination uncertainty, and actual binary dispatch.
Run `cargo test --locked --test search_service -- --test-threads=1` through the repository's normal
validation environment. Test definitions alone are not execution evidence.
