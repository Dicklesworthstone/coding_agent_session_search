# Supervised one-shot lexical search

`cass serve --query QUERY` runs one lexical search and exits. It does not read
stdin, start maintenance, download models, or launch another process. This uses
the same read-only request handler as `cass serve --stdio`, without requiring a
JSON-lines client or pipeline.

```sh
cass serve --data-dir /absolute/path/to/cass-data \
  --query "git fixture" --limit 5 \
  --request-timeout-ms 10000 --max-resident-mib 2048
```

Use either `--data-dir` or an explicit published `--index` directory. `--query`
and `--stdio` are mutually exclusive. Output is always the service's versioned
JSON reply; no `--json` flag is needed. This is not the ordinary `cass search`
output schema. Ordinary search remains unchanged.

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
without the configured admission policy.

## One deadline and truthful exit status

For `--query`, a single watchdog starts after argument validation, before path
resolution and the first resident-memory probe. It covers reader admission,
index open, primary search, optional recovery, reader and bridge teardown,
serialization, and response flushing. None of these stages gets a fresh
request timeout. No successful result is published until readers and bridge
runtimes have been released. The sampled resident-memory supervisor remains
active through output; it is not a kernel-enforced allocation limit.

Successful replies, including authoritative empty results, go to stdout and
exit zero. Search failures and oversized replies go to stderr, leave stdout
empty, and exit 1. Invalid query/page/filter values exit 2. The shared watchdog
and memory-supervisor exit codes remain 124 (deadline), 125 (supervisor failure)
and 126 (sampled memory threshold). Incomplete bridge teardown also exits 125.
Persistent stdio mode likewise shuts down its cached bridge runtimes before
its teardown deadline and memory sampler stop. A forcibly terminated worker need not emit
a final JSON diagnostic. CLI syntax errors retain Clap's normal diagnostics.

## Filters and bounds

`--limit` defaults to 10 and accepts 1 through 100. `--offset` defaults to zero;
`offset + limit + 1` must not exceed 1024. Query text is limited to 4096 UTF-8
bytes. Queries plus filter identities have a 64 KiB aggregate input limit;
encoded output retains the service's 1 MiB bound.

Query-only filters are repeated `--agent` (exact stored slug), repeated
`--workspace` (exact workspace), `--source-id` (one literal source ID), and
`--created-from` / `--created-to` (inclusive epoch milliseconds). They use the
same validation as stdio requests. They do not add session-path post-filtering,
synthetic source groups or agent-alias normalization. Semantic and reranker
startup options are rejected with `--query` rather than silently ignored; use
stdio requests for those opt-in operations. Reader-pool admission settings are
available in either mode.

Regression coverage: `cargo test --bin cass search_service::once::tests` and
`cargo test --test search_service_once`. These include real index/archive
fixtures, open-stdin completion, explicit fuel recovery, read-only checks,
authoritative empties, validation-before-storage, bounded serialization and
nonzero failure exits. Unix lifecycle tests also inject a pre-filled output
socket and require exit 124 plus released pool locks in one-shot and stdio modes.
Test presence is not evidence that a particular build
has passed them.
