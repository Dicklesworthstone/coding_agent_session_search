# Supervised one-shot search and transcript pages

`cass serve --query QUERY` runs one lexical search by default and exits.
`--mode semantic` or `--mode hybrid` selects the existing opt-in semantic
service. `cass serve --transcript JSON` reads one canonical transcript page.
None of these modes reads stdin, starts maintenance, downloads models, or
launches another process. They use the same read-only request handlers as
`cass serve --stdio`, without requiring a JSON-lines client or pipeline.

```sh
cass serve --data-dir /absolute/path/to/cass-data \
  --query "git fixture" --limit 5 \
  --request-timeout-ms 10000 --max-resident-mib 2048
```

Use either `--data-dir` or an explicit published `--index` directory. `--query`,
`--transcript`, and `--stdio` are mutually exclusive. Output is always the
service's versioned JSON reply; no `--json` flag is needed. This is not the
ordinary `cass search` output schema. Ordinary search remains unchanged.

## Semantic and hybrid queries

```sh
cass serve --data-dir /absolute/path/to/cass-data \
  --db /absolute/path/to/cass-data/agent_search.db \
  --semantic-embedder minilm --mode hybrid \
  --query "How was authentication caching fixed?" --limit 5
```

Nonlexical modes require explicit `--semantic-embedder`, `--data-dir`, and
`--db`. Local neural choices require installed models and compatible vectors.
An explicit external embedder retains its separate endpoint-consent policy;
selecting semantic mode does not grant that consent. Hash vectors are never
silently substituted for a requested neural provider.

Semantic-only failures remain errors. Hybrid may fall back to lexical results,
with `realized_mode` and `semantic_fallback_reason` describing what ran.
`--approximate` requires explicit semantic/hybrid mode; the service currently
uses exact retrieval and reports native ANN as unavailable. Merely configuring
an embedder does not change the default lexical query. See
[the retained service contract](SEARCH_SERVICE.md) for admission and snapshot
limitations.

## Canonical transcript pages

Use the exact source and conversation coordinates returned by a search hit:

```sh
cass serve --data-dir /absolute/path/to/cass-data \
  --db /absolute/path/to/cass-data/agent_search.db \
  --transcript '{"source_path":"/history/session.jsonl","source_id":"local","conversation_id":42,"limit":16}'
```

The JSON selector requires `source_path`, `source_id`, and `conversation_id`.
Optional fields are `limit` and `cursor`. Do not include a wire-protocol `id`
or `op`; the one-shot reply uses ID 1. Unknown fields, duplicate keys, invalid
coordinates, and mixed search options are rejected. Paths are compared as exact
identity strings, not resolved or opened as transcript files. `--db` must be
explicit. A service location (`--data-dir` or `--index`) is still required, but
transcript retrieval never opens its lexical index or a model.

`limit` belongs inside the selector and counts message chunks, not search hits.
It defaults to 16 and accepts 1 through 32. A page contains at most 32 KiB of
message content, preserving UTF-8 and embedded NUL characters. Large messages
continue across pages rather than becoming truncated previews. Returned chunks
carry message IDs, one-based message indices, byte offsets, full message byte
lengths, and completeness flags. These indices are not raw-file line numbers.

For the next page, pass the **entire** `result.next_cursor` object as the
selector's `cursor`, retaining the same source path, source ID and conversation
ID. Repeat only while the cursor is non-null. No automatic unbounded walk is
started. `end_of_conversation: true` marks the end observed by that page.

**Pages are not one immutable export.** Each invocation has a separate archive
read transaction. Cursors validate message identity and length; same-length
edits between pages are not detected. Deleted/replaced/resized cursor messages
fail instead of silently selecting a neighbour. Responses explicitly retain
`cross_page_snapshot: false` and `cursor_validation: "message_identity_and_length_only"`.
Use a verified logical archive for a coherent whole-session export.

## Explicit small-archive recovery

```sh
cass serve --data-dir /absolute/path/to/cass-data \
  --query "git fixture" --limit 5 \
  --db /absolute/path/to/archive.db --lexical-fallback \
  --request-timeout-ms 10000 --max-resident-mib 2048
```

The existing fallback admits at most 64 MiB of database plus WAL. Only a Quill
query-fuel refusal can enter it. Merely specifying `--db` is not consent, and a
successful zero-hit Quill result never opens the fallback archive. No fuel
allowance is raised and no index is rebuilt. A changed backend is reported as
`backend: "frankensqlite"`, `degraded: true`, with fallback metadata. These results
are fresh archive reads, not the pinned Quill snapshot; ranking and prefix
semantics are not equivalent. Larger archives are refused rather than scanned
without the configured admission policy. This recovery policy is for lexical
search, not the separate canonical transcript reader.

## One deadline and truthful exit status

For each one-shot operation, a single watchdog starts after argument validation,
before path resolution and the first resident-memory probe. It covers reader
admission, storage access, retrieval, optional recovery, reader and bridge
teardown, serialization, and response flushing. None of these stages gets a
fresh request timeout. No successful result is published until readers and
bridge runtimes have been released. The sampled resident-memory supervisor
remains active through output; it is not a kernel-enforced allocation limit.

Successful replies, including authoritative empty results, go to stdout and
exit zero. Retrieval failures and oversized replies go to stderr, leave stdout
empty, and exit 1. Invalid query/page/filter/selector values exit 2. The shared
watchdog and memory-supervisor exit codes remain 124 (deadline), 125 (supervisor
failure) and 126 (sampled memory threshold). Incomplete bridge teardown also
exits 125. Persistent stdio mode likewise shuts down its cached bridge runtimes
before its teardown deadline and memory sampler stop. A forcibly terminated
worker need not emit a final JSON diagnostic. CLI syntax errors retain Clap's
normal diagnostics. Transport failure can leave a partial output frame; only
accept a complete reply together with successful process exit.

## Filters and bounds

For searches, `--limit` defaults to 10 and accepts 1 through 100. `--offset`
defaults to zero; `offset + limit + 1` must not exceed 1024. Query text is limited
to 4096 UTF-8 bytes. Queries plus filter identities have a 64 KiB aggregate
input limit; encoded output retains the service's 1 MiB bound. Transcript
selectors are bounded at 64 KiB of encoded JSON before deserialization, including
whitespace and escapes. The 32-KiB transcript content limit is separate from
the encoded output limit and does not bound the engine's internal allocations.

Query-only filters are repeated `--agent` (registered slug or supported alias),
repeated `--workspace` (exact workspace), `--source-id` (one literal source ID),
and `--created-from` / `--created-to` (inclusive epoch milliseconds). They use
the same validation and agent normalization as stdio requests. They do not add
session-path post-filtering or synthetic source groups. Candidate reranking
still requires stdio requests; `--reranker-model` conflicts with one-shot modes.
Transcript limits/cursors belong in its JSON selector; query flags must not
silently narrow or widen canonical retrieval. Reader-pool admission settings
are available in all modes.

Regression targets include the binary's `search_service::once::tests` and the
integration targets `search_service_once`, `search_service_once_semantic`, and
`search_service_once_transcript`. These use real index/archive fixtures,
open-stdin completion, explicit recovery, read-only checks, authoritative
empties, validation-before-storage, bounded serialization and nonzero failure
exits. Transcript process tests compare stdio results, reassemble Unicode/NUL
content across fresh processes, and reject changed cursor messages. Unix
lifecycle tests also inject a pre-filled output socket and require exit 124
plus released pool locks in one-shot and stdio modes. Run these through the
repository's batched RCH gate; test presence is not evidence that a particular
build has passed them.
