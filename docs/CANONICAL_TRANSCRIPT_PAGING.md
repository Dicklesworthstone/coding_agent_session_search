# Canonical transcript paging in the persistent search service

`cass serve` exposes `transcript` over its JSON-lines protocol and
`cass_transcript` over MCP when started with an explicit `--db`. This lets a
caller read a conversation forward, including individual message bodies that
exceed the existing `view`/`view_batch` 64 KiB content allowance. It does not
change those operations' complete-window contracts.

```sh
cass serve --stdio --data-dir /path/to/cass \
  --db /path/to/cass/agent_search.db
```

Copy `source_path`, `source_id`, and `conversation_id` from one search result.
They must exactly identify the same canonical conversation. The source path is
an identity string: the service never opens that provider file. The index need
not exist for transcript requests, and no model, index, network service, or
provider is loaded. Only the fixed startup database is opened, strictly
read-only. `--db` is required even when a data directory has been supplied.

Send a first JSON request with no cursor (or with `cursor: null`):

```json
{"op":"transcript","id":1,"source_path":"/history/rollout.jsonl","source_id":"local","conversation_id":7,"limit":16}
```

The response follows the usual `ok`/`result` envelope. A successful result has
`messages`, `count`, `content_bytes`, `next_cursor`, and `end_of_conversation`.
Continue with the exact returned `next_cursor` and the same source/conversation
coordinates. A null cursor with `end_of_conversation: true` ends this traversal.
Passing null again starts a new traversal; there is no retained server cursor.

## Content, ordering, and limits

Every page returns at most **32 KiB of UTF-8 content** and **32 message chunks**.
`limit` defaults to 16 and accepts 1 through 32. It bounds chunks, not characters
or SQL offsets. Empty messages count toward the message limit. The reader uses
canonical message-index order and preserves gaps rather than inventing missing
messages or interpreting indices as provider-file line numbers.

Each item contains `message_id`, the one-based `message_index`, `role`, and
`content`, with `byte_offset`, `end_byte_offset`, `content_bytes`,
`message_content_bytes`, `message_complete`, and `partial_message`.
`agent` roles are normalized to `assistant`, as in canonical `view`.

An oversized message is split across pages at valid UTF-8 boundaries. Embedded
NULs and complete Unicode code points are preserved. Append chunks of the same
`message_id` only at their stated contiguous byte offsets; keep going until
`message_complete` is true. A final chunk may still have `partial_message: true`
because it starts after byte zero. Neither it nor an earlier chunk is the
complete message by itself. Invalid text is refused, not replaced or omitted.

The cursor contains the next `message_id`, `message_index`, `byte_offset`, and
`content_bytes`. For a partially emitted message, `content_bytes` binds its
original total byte length. Before the next unread message it is null. Unknown
cursor fields, invalid ranges, and inconsistent offsets are rejected. The next
observed message must match the cursor's exact ID/index; a deleted or replaced
anchor is not silently substituted with a surviving neighbour.

The allowance bounds content transferred into the response, not the database
engine's allocations, page I/O, or process RSS. SQL body slices and an extra
metadata anchor are bounded independently. The service's whole-request deadline,
cooperative canonical lookup budget, response-frame limit, shared reader
admission, and sampled resident-memory supervision remain in force. A failed
page returns no partial page result; pages already returned remain the caller's.

## Snapshot semantics

**A page is one read transaction; a sequence of pages is not one immutable
snapshot.** No database transaction or cursor-owned lease remains open while the
caller is idle. Responses state `cross_page_snapshot: false` and
`cursor_validation: "message_identity_and_length_only"`.

A changed ID or changed length of a partial message yields
`canonical_cursor_changed`. Same-length body edits, changes behind the cursor,
and a same-identity/same-length database replacement between requests are not
certified or detected by this protocol. Restart paging after a cursor failure.
The result does not certify that an earlier lexical search was fresh.

For a coherent whole-conversation extraction, use `archive export` to create a
verified logical backup, then `archive extract-conversation` on that backup.
Transcript paging returns message bodies, roles, and coordinates, not all
canonical metadata or a restorable archive.

## MCP

Add `--mcp` at service startup, perform the existing MCP handshake, then call
`cass_transcript` with the same selection, limit, and optional cursor fields.
There is no `op` or `id` inside MCP tool arguments. Tool discovery is passive and
the tool is absent without startup `--db`. Its annotation remains read-only and
local even when a separately configured semantic tool uses an external provider.
Both MCP result representations carry the same content and continuation data.

The implementation includes native reader, protocol, MCP, and real-binary
JSON/MCP regressions. Their inclusion is not runtime qualification: the new Rust
tests have not run in the editing environment, which has no Rust toolchain.
