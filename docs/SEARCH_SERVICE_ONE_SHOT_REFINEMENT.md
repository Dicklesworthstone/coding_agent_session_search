# Supervised one-shot lexical refinement

`cass serve --refine JSON` retrieves a bounded lexical shortlist, reranks its
titles and previews with an explicitly selected installed local cross-encoder,
then exits. It does not read stdin or require a client to implement the stdio
protocol. It uses the same `refine` request and production ranking code as the
persistent service, not a new search backend.

```sh
cass serve --data-dir /absolute/path/to/cass-data \
  --reranker-model /absolute/path/to/installed-reranker \
  --refine '{
    "query": "Which change fixed the test isolation failure?",
    "lexical_query": "git AND (fixture OR isolation)",
    "candidate_limit": 20,
    "limit": 5,
    "filters": {
      "agents": ["codex"],
      "workspaces": ["/absolute/path/to/project"]
    }
  }'
```

`--index /absolute/path/to/published-index` can replace `--data-dir`. The
reranker directory must contain the local model files accepted by the existing
service loader. This command does not install or download a model. A fresh
one-shot invocation must load it again when candidates need scoring; use the
persistent stdio/MCP service to retain models and readers across requests.

## Selection and limits

`query` is the natural-language question given to the reranker. `lexical_query`
is the separate lexical expression used to find candidates; both are required,
nonempty and limited to 4,096 UTF-8 bytes each. The complete encoded JSON
selection is limited to 64 KiB before decoding, including whitespace and escapes.
Unknown or duplicate fields are errors.

`candidate_limit` defaults to 20 and must be between 1 and 32. `limit` defaults
to 5 and must be between 1 and `candidate_limit`. Specifying a candidate limit
below five therefore also requires an appropriate explicit result limit.
There is no offset: changing the candidate pool can reorder all refined hits,
so the result's `next_offset` is null.

Optional `filters` have the same semantics as service search: `agents`,
`workspaces`, `source_id`, `created_from` and `created_to`. Agent aliases are
resolved by the shared connector registry; unknown agents are rejected rather
than ignored. Workspace strings and source IDs retain their literal scope.
Creation timestamps are inclusive epoch milliseconds. The existing limits of
32 values per agent/workspace filter and 4,096 bytes per identity apply.
Session-path filters and other unsupported fields are rejected.

Put all retrieval controls inside the selection. `--query`, `--transcript`,
`--stdio`, `--mcp`, `--mode`, `--approximate`, scalar query flags such as
`--agent`/`--limit`, and `--semantic-embedder` cannot be combined with
`--refine`. Ordinary `--query` does not become reranking merely because a
model directory was supplied.

## What the result means

This is **candidate-only refinement**, not global semantic retrieval. The model
scores only titles and bounded lexical previews, not full canonical messages.
Existing inference admission limits remain in effect: at most 8,192 bytes per
candidate preview and 256 KiB for combined candidate/query input. Model input
may also be token-truncated by the loader's existing configuration.

The reply preserves complete service hit identities and their original lexical
`score`; `lexical_rank` and `rerank_score` identify the refinement separately.
Equal reranking scores retain lexical order. The response reports
`ranking_scope: "bounded_lexical_candidate_pool"`, candidate counts, model
state and the actual lexical backend. Empty shortlists return
`refinement.status: "no_candidates"` without opening the model. A model load or
inference failure is an error, not a successful lexical result relabeled as
reranked.

## Storage and supervision

The normal path needs only a published lexical index and the local reranker.
It does not require `--db`, open raw session files, hydrate full messages, load
semantic vector assets, or start indexing/maintenance. The separate explicit
`--lexical-fallback --db PATH` option remains available for Quill fuel refusals
on small archives; its existing changed-backend and ranking limitations remain
visible in the refinement response.

The one-shot request shares a single `--request-timeout-ms` deadline across
reader/model setup, retrieval, inference, optional recovery, teardown and output.
The resident-memory sampler and optional cross-process reader admission pool
remain active. The memory sampler is not a kernel allocation ceiling or an
instantaneous peak-RSS guarantee.

Successful JSON is written to stdout only after readers, models and bridge
runtimes have been torn down. Errors use the existing one-shot stderr envelope
and nonzero exit status. The encoded response remains bounded to 1 MiB; no
partial success is substituted after a validation or scoring failure.
