# GH538: October 31 watch ingestion follow-up

## Scope and evidence

This work starts from main `30f96a3dbed4f18e99ff205541fa97528eeba3e7`, the exact
revision used in [jwross24's October 31 report](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/538#issuecomment-6539798177).
The earlier [watch reliability investigation](issue-538-watch-reliability.md)
and its bounded spool, notification backlog, retry queue, raw-mirror admission,
diagnostics, cancellation, and watchdog fixes remain in place.

The report separates two failures on one macOS host with approximately 8,640
conversations and 697,000 messages. Default launchd scheduling correlated with
filesystem stalls; setting `ProcessType=Interactive` let the same binary finish
standalone indexing in approximately 30–191 seconds. A shell-launched watcher
avoided those stalls but did not make new conversations or appended messages
searchable during the observed windows. A subsequent standalone run reported
five conversations and 10,664 messages. The raw mirror exceeded its default
8 GiB capture limit and emitted warnings. These observations do not prove which
filesystem syscall blocked or that mirror admission caused missed ingestion.

The open Beads liveness epic
`coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.4` already
requires durable retries and a distinction between heartbeat and progress.
Related tasks `362yj` (GH515 engine admission), `3zq55` (mirror read
amplification), and `ayhvu` (build-wide ingest-contract invalidation) were
reviewed. This change does not close those separate investigations. Work and
remaining qualification are tracked by
`coding_agent_session_search-gh538-watch-ingestion-20261031`.

## Confirmed source-level failure paths

The live callback scanned a selected connector root using a shared provider
timestamp. It persisted root-wide lexical replay debt before canonical writes.
An active source held the watermark and prevented clearing that debt even when
the admitted conversations had all published successfully. The next retry then
disabled the timestamp cutoff and replayed canonical history across the root.
Repeatedly processed old conversations and per-chunk timestamp updates could
look like progress while the next new source waited behind that work.

Startup already had a durable source ledger. Live watch callbacks did not use
it. Reusing that contract makes unchanged published sources cheap to revisit
and keeps an active sibling from forcing the whole archive through parsing and
lexical publication on every retry. This is a demonstrated code path, not a
claim that the reporter's original archive has been reproduced here.

Two additional gaps were found. Notification registration happened after the
startup census, leaving writes between those operations without a guaranteed
event. Also, status and health did not inspect durable watch replay debt, so a
live lease and recent timestamp could report readiness despite known pending
ingestion or publication recovery.

## Ingestion and recovery changes

The implementation is committed in
[`8b5441a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b5441a955d9ed50f05ee9c92506e1e4c6fe1a1f),
following the independent readiness correction in
[`e69fe41`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e69fe41b634a51a914697af81a3f951b39cd9263).

For connectors with source boundaries, discovery uses source observations
instead of a provider-wide time cutoff. This includes imported files with old
mtimes and changes made while the watcher was stopped. Reuse still requires the
existing producer contract, source identity, primary metadata, and required
sidecar evidence. Explicit force-full and watch-once behavior remain available;
semantic watch-once continues to count reconciled sources for its attestation.

A watch receipt includes `watch_published: true` only after every admitted
conversation from that stable source has passed canonical persistence and a
successful lexical commit. A startup receipt alone cannot skip a source while
older root-wide publication debt is being recovered. Before another attempted
mutation, invalidating admitted receipts and establishing publication debt occur
in one metadata transaction. The final receipt and the debt it discharges also
commit together. A crash or injected publication failure therefore cannot leave
an old receipt certifying unfinished new work.

Older root debt remains conservative across exclusions, active sources,
quarantines, and incomplete scans. When such debt cannot finish, one final
admitted source remains unacknowledged so a future callback cannot reuse every
source and strand recovery with no ingest batch. A preparse mirror deferral of
an unchanged, already-published source does not prevent its sibling's successful
recovery from completing.

Discovery retries are distinct from unpublished canonical mutations. A new
`discovery_only` flag, omitted for ordinary and legacy debt records, allows a
clean all-reused census to discharge a prior discovery error. Discovery-only
markers still make readiness incomplete. They do not force already-published
sources through lexical replay. A file observed changing while parsed is queued
again. The check runs at source transitions and scan end as well as completion,
because the pinned Codex and Claude implementations suppress completion when
they detect a changed source.

The watcher seeds one reconciliation of configured roots **after** registering
filesystem notifications. It uses the existing bounded pending queue, cooldown,
overflow behavior, and spool; no new polling thread or unconditional archive
rebuild is introduced. An ordinary source-receipt scan can now recover a
backdated source even without the overflow fallback. The overflow regression
still checks the deliberate force-full path and preservation of old identities.

Publication counts now count committed searchable work. An unchanged retry
does not commit an empty lexical generation, advance `last_indexed_at`, or wake
the optimizer. Partially admitted work retains conservative watermarks. This
separates processing activity from publication evidence without altering the
watchdog's real activity boundaries or its terminal abort behavior.

## Resource and identity bounds

The new live-watch receipt cache reads metadata in pages of at most 256 rows.
It examines at most 65,536 bounded keys, retains at most 64 MiB of key/value
payload, and suppresses values exceeding 256 KiB before converting them to Rust
strings. UTF-8 byte length, rather than character length, governs this limit.
The cache is released immediately after scanning.

Completion receipts have separate count, aggregate payload, and per-entry
serialization limits. Reused-path diagnostics and dependency-path observations
also have count and byte limits. If the bounded invalidation set fills, the
metadata transaction conservatively invalidates the receipt namespace before
canonical mutation. An omitted optimization receipt causes ordinary parsing;
it does not discard a conversation. These are bounds on the added receipt
buffers, not a constant-memory guarantee for the SQL engine or each upstream
connector's discovery implementation. The existing transcript spool and its
disk-space reserve remain unchanged.

Source-key serialization is fallible. An unencodable native path disables reuse
and receipt creation while allowing ordinary parsing. Distinct paths are not
collapsed through lossy Unicode conversion, and representable existing keys
remain byte-compatible. Missing evidence by itself does not create an endless
mutation retry.

## Readiness and launchd

`pending.lexical_replay` is additive: `true` means durable watch recovery is
present, `false` means a successful bounded query found none, and `null` means it
was not established. The indexed metadata range query uses `LIMIT 1` and does
not parse recovery payloads. Even malformed debt remains pending. Known debt
prevents a ready/fresh verdict and marks the index partial; a requested probe
that fails cannot certify readiness. Existing stronger error, missing, hollow,
and building states remain intact. `pending.sessions` retains its existing
meaning; recovery roots are not invented session counts.

[Commit 7db0e09](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7db0e0902cceae5052d42947955ea71fe230cbd0)
sets `ProcessType=Interactive` in generated launchd jobs. CPU niceness and the
existing load/idle admission gates remain. This is consistent with the
[launchd ProcessType documentation](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5),
and addresses a concrete generated-plist omission. It does not rewrite a
reporter's custom plist or prove the cause of either reported failure.

Optional raw-mirror maintenance now runs inside the index owner's observed
`watch_startup:raw_mirror_maintenance` preflight, after its lease and watchdog
exist. It previously ran from `main` before tracing and the indexing watchdog.
The typed existing index lease preserves index-before-mirror lock order without
reacquiring the same lock. Retention planning checks cancellation between bounded
reads; mandatory audit, publication, and durability operations remain observed
without introducing unsafe cancellation points. Capture-cap refusal continues
to warn and allow ingestion.

The CLI worker explicitly admits that maintenance. Generic library, TUI,
refresh, and doctor indexing retain their previous behavior and do not begin
automatic retention merely because the helper moved into the index owner.

## Regression coverage and qualification status

The new live test
`deferred_watch_sources::gh538_live_watch_ingests_startup_append_and_restart_over_mirror_cap`
uses real Codex and Claude files and a real pre-existing index/mirror. It runs
the watcher with a one-byte capture ceiling, requires actual capture-paused
warnings, and checks 16 search sentinels across startup changes, new nested
directories, successive appends, downtime changes, and a second watcher. Final
assertions require seven canonical conversations and 23 ordered messages,
exactly one search hit per sentinel, retained repeated assistant content,
unchanged mirror bytes/mtimes/generation, no stall, and a ready live watcher.
The only standalone index command bootstraps the fixture before watch work.

The new library lifecycle tests use 128 existing real sources plus an
advisory-locked source for each provider. They require no-op retries to reuse
history without changing manifest bytes or publication timestamps, a new closed
source to publish while another remains locked, and append/reopen/backdated
import recovery without duplicate canonical rows or lexical identities. Other
tests cover discovery failure, an already-published locked sibling, and
`PublicationAfter(0)`, `Commit`, and post-commit `DebtClear` failures. Health
tests use real canonical connections, lexical generations, and a held lease.
Cache, source identity, mutation, retention cancellation, and scheduler tests
cover the corresponding boundaries.

The executed source/component selections passed against the frozen production
sources: **19 mirror tests, 32 watchdog tests, 24 source-boundary tests, and four
launchd renderer tests**. One native matcher benchmark was deliberately ignored.
The mirror and scheduler negative controls reproduced their specified baseline
failures. Four emitted plists also passed independent XML/plist checks. The
mirror fixture used 20,816 files, 9,523 manifests, and 11 GiB of sparse logical
staging data; it is not a physical-disk or APFS throughput measurement. The
watchdog suite included a real blocked FIFO and required exit 70.

Pinned UBS v5.4.4 completed the 17 Rust targets and the Python harness. It
remains red with findings. Changed-line critical Rust findings were intentional
regression-test failure branches. The three newly introduced Git subprocess
timeout findings were fixed and a Python-only recheck confirmed their removal;
remaining Python findings concern pre-existing Cargo subprocess paths. No
scanner suppression was added. Actual AST execution was observed, but the
pinned helper does not record every AST subprocess outcome, so complete AST
coverage is unproven. Cargo-backed scanner checks were not evaluated after the
separate dependency compilation failures.

**Full application qualification remains open.** In this environment both
`cargo test --offline --locked --test watch_e2e --no-run` and
`cargo check --offline --locked --all-targets` exited 101 after the compiler for
pinned `asupersync 0.5.0` was killed with SIGKILL. Neither reached CASS. They used
the pinned nightly, one build job and compiler thread, no debug information,
no incremental compilation, and the unmodified repository dependency graph.
The full application tests above were added and reviewed, not executed here.

Formatting and supplemental source-test results, exact source identities,
commands, scanner status, and exclusions are recorded separately in
[`issue-538-oct31-validation.json`](issue-538-oct31-validation.json). Component
tests do not certify canonical/lexical transaction recovery, the complete
watcher, macOS notifications, APFS behavior, or the reporter's archive.

On a provisioned build host, run the repository gate without weakening its
checks. A focused selection for this work is:

```sh
GATE_BUILD_JOBS=1 scripts/gate.sh --local \
  --lib-filter 'gh538 watch_source_ledger source_ledger_tests raw_mirror schedule' \
  --integration 'watch_e2e:gh538,cli_index:gh439'
```

Then repeat the live ingestion and held-lock/watchdog regressions natively on
macOS under shell and generated Interactive launchd jobs. Retain actual search
results and canonical counts, not just process liveness or progress output.
The original launchd syscall attribution and full-archive performance remain
unverified until that evidence exists.
