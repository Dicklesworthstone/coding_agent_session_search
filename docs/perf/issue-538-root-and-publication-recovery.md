# GH538: root registration and publication recovery

Investigation date: 2026-10-31. Baseline:
[`5cfb9d6a033927255a8a4befad153a44c8ed36e4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5cfb9d6a033927255a8a4befad153a44c8ed36e4).

## Scope and qualification

[Issue #538](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/538)
and [the October 31 reporter evidence](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/538#issuecomment-6539798177)
describe a shell-launched watcher that remained active while new conversations
and appended messages stayed unsearchable. Standalone indexing subsequently
ingested the missing content. The reporter separated this behavior from the
large launchd scheduling difference on the same installation.

This follow-up investigates additional failure paths after the
[October 31 ingestion changes](issue-538-oct31-ingestion.md). Two native watcher
failures have been reproduced with isolated directories on Linux. The canonical
publication regressions and complete CASS watcher tests are implemented but
**full application qualification remains outstanding after dependency code
generation exceeded the available memory**. A metadata-only compile check is
continuing separately; it does not execute the application tests.
No reporter archive was accessed. These results do not establish the
cause or resolution of the reporter's seven probes, macOS FSEvents behavior, or
original-archive performance. The GH538 ingestion bead remains open.

## Failure paths and corrections

### Failed or obsolete native registrations

The baseline logged a failed initial `watch()` registration without retrying it.
A configured directory created later could therefore remain invisible. A
directory replacement presented another case: a native watch could continue
following the old filesystem object while the configured pathname named a new
directory. A live watcher lease did not establish delivery from that replacement.

The candidate retains missing explicitly configured roots for continuous watch
discovery and checks distinct configured roots at the existing watch interval.
On Unix, registration identity includes device, inode, and file/directory type.
Missing, failed, and replaced registrations are retried. A successful rearm
queues an ordinary source-receipt census to cover the registration gap; an
extant root whose registration still fails also receives that fallback census.
Healthy unchanged registrations schedule no census.

Shared native paths use one registration with the strongest required mode while
retaining all source roots for classification. Database roots continue to watch
their parents for sidecars. A database that appears beneath an already watched
parent joins reconciliation and the existing database `PollWatcher`, including
WAL writes through a descriptor that stays open. Missing-root retention applies
to continuous watch discovery; standalone discovery retains its existing policy.
See `refresh_watch_root_registrations`, `database_root_poller`, and `watch_sources`
in [the indexer](../../src/indexer/mod.rs).

A follow-up native reproduction exposed overlapping registration ownership.
Two configured paths can resolve to the same inode, and a recursive parent can
already own a descendant watch. Removing one overlapping registration can
disable another on inotify. Native targets are now canonicalized and recursive
coverage is coalesced before registration. The configured source paths remain
attached for classification, provenance, and reconciliation. Retargeting a
symlink therefore enrolls its new target without removing the original root's
shared native registration.

Each configured source also retains its resolved-path and filesystem-object
binding independently of that shared native registration. An alias outside a
watched parent can change from one already-existing child to another without
an event beneath that parent. Its changed binding now requests a census even
when the native parent registration and source membership are unchanged.

For the FSEvents backend, any native watch/unwatch attempt also requests an
ordinary census of every extant configured source root. The pinned notify 8.2.0
implementation stops and restarts its entire stream from `SinceNow` on those
operations, including failed attempts, which can hide a concurrent write to an
otherwise unchanged root. This safeguard follows dependency-source inspection;
it is not a claim of native macOS runtime validation. Healthy registration
checks still schedule no census.

### Canonical writes whose raw transcript disappears

The source audit found a separate publication obligation:

1. A canonical append commits, but lexical publication fails. Durable root debt
   remains and the old source receipt has been invalidated.
2. The producer moves or removes the raw transcript before retry.
3. Discovery no longer emits its conversation ID. An empty retry cannot obtain
   that ID for replay; a retry with a changed sibling can otherwise complete
   without publishing the absent source's canonical messages.

For a complete eligible census with publication debt, the candidate visits
canonical identities for the selected connector and source ID in bounded pages.
It normalizes legacy path spellings and reconciles missing sources within the
intersection of the current root and debt scope using the existing canonical
replay path. Claude's
canonical and legacy provider aliases are included. Existing files remain under
ordinary source ingestion. This recovery runs before both empty and nonempty
sibling batches can discharge the old obligation.

An older parent obligation can therefore repair a currently configured child
after configuration narrows. That child census cannot clear the ancestor debt
or replay former siblings; a later census covering the whole debt must itself
be authorized by the current roots. Missing-leaf promotion keeps its existing
deepest-root selection and does not broaden configuration to finish old debt.

Recovery starts from the committed lexical generation and commits bounded groups.
Debt survives failed publication; successful retry preserves stable message
identities and historical results. It creates no raw-source publication receipt
for an absent file. An empty census must also establish coverage of its existing
canonical siblings through reused publication receipts before clearing debt.

The actual notification retry needed an additional correction. A failed callback
retains its original leaf path; after that leaf disappears, ordinary
`classify_paths` cannot stat it. `promote_missing_watch_replay_paths` now selects
the deepest current, extant owning directory only when the missing path overlaps
durable debt for the same connector and source. It reads debts once when a
definitively missing pending path exists. This makes same-process retry possible
without another event. Explicit watch-once classification is unchanged.

## Preserved behavior and limits

The candidate builds on the existing changes rather than replacing them:

- [`7db0e090`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7db0e0902cceae5052d42947955ea71fe230cbd0)
  selects Interactive process type for generated launchd jobs.
- [`e69fe41b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e69fe41b634a51a914697af81a3f951b39cd9263)
  makes known durable replay debt incompatible with healthy readiness.
- [`8b5441a9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b5441a955d9ed50f05ee9c92506e1e4c6fe1a1f)
  supplies source observations, transactional receipt invalidation/publication,
  startup reconciliation after registration, and deferred-source retries.

Optional raw-mirror capture refusal remains a warning that permits canonical
and lexical ingestion to proceed. A mirror above its cap is not, by itself,
evidence of the missing-ingestion cause. The existing over-cap application
regression and the new root-replacement regression preserve prior mirror bytes,
mtimes, and generation while requiring searchable updates.

Root probes inspect configured roots and required database parents, without
walking healthy archives. Failed registrations can request an ordinary census
at the existing cooldown; that census can still enumerate a large root. Replay
identity pages contain at most 256 rows and use the existing message-chunking
path. Its per-conversation message-footprint vector remains resident. These are
specific buffer and scheduling bounds, not a constant total RSS or archive-I/O
guarantee. Activity ticks follow completed operations and do not make a blocked
syscall responsive.

Active, unstable, excluded, and otherwise incomplete recovery scans retain
their obligations. An absent canonical conversation with zero messages also
retains debt: the existing replay API cannot prove deletion of stale lexical
documents by replaying no messages. Successful siblings do not advance that
incomplete root's publication-completion timestamp or persistent watermarks.

Two adjacent gaps remain explicit:

- Canonical recovery here covers vanished children beneath an extant current
  root. A wholly absent configured root remains pending until it returns.
- An empty canonical conversation cannot certify removal of stale lexical
  documents through the existing upsert-only replay path. Its debt remains
  visible; this condition alone does not schedule an endless retry loop.

The follow-up also closes the premature-completion path for an existing source
that emits zero conversations. Pinned Codex and Claude can omit their source
completion callback for empty or otherwise unreadable inputs, so sibling
success cannot certify that source. Recovery now captures bounded coverage
from validated source completions and reused publication receipts, and requires
that coverage for every extant canonical identity in the selected debt scope.
Uncovered sources preserve debt, timestamps, and both watch watermarks.

An uncovered extant source with nonempty canonical history retains a retry on
the existing watch cooldown. This matters when the original notification has
already been consumed. Healthy siblings can still publish their own receipts,
so subsequent retries do not republish their history. A complete later read can
finish publication through the retained queue, without another notification.
Coverage is conditional on all admitted canonical and lexical batches actually
committing; merely emitting a conversation is not completion evidence.

## Regression evidence

The native component harness executes the selected production watch loop,
notification backlog, event filter, registration, database polling, and tests
verbatim with dependencies matching the repository lock. Its small
`ConnectorKind`, `ScanRoot`, and atomic-progress adapters omit other application
state and SQL cancellation. Callbacks inspect source bytes. They do not persist
canonical conversations or search lexical results.

| Selection | Baseline | Candidate | Evidence class |
| --- | --- | --- | --- |
| Failed initial root registration | Callback-observation timeout | Passed | Native component |
| Replaced native root | Replacement-append observation timeout | Passed | Native component |
| Healthy root does not schedule periodic scans | Not a baseline negative control | Passed | Native component |
| Shared-path coalescing ignores ordinary directory mtime changes | Not a baseline negative control | Passed | Native component |
| Late database joins discovery and WAL polling | Not a baseline negative control | Passed | Native component |
| Retargeted alias retains the original shared root | Intermediate candidate failed | Passed | Native component |
| Recursive parent and nested alias preserve native coverage | Not a baseline negative control | Passed | Native component |
| Alias retarget within an unchanged native parent requests census | Intermediate candidate failed | Passed | Native component |
| Canonical recovery and actual pending-leaf retry | Regression tests prepared | Pending execution | Full application library |
| Live replacement, over-cap updates, and restart | Regression tests prepared | Pending execution | CASS subprocess and search |

The baseline pair, `gh538_native_watch_retries_failed_root_registration` and
`gh538_native_watch_rearms_replaced_root`, failed **2/2** in 30.03 seconds. The
initial component selection passed **5/5** in 21.51 seconds. The final follow-up
selection passed **8/8** in 20.08 seconds; these times cover test fixtures and
waits, not indexing throughput. Session evidence is retained in `registration-baseline.log`,
`registration-final.log`, and `registration-probe/sources.json` beside the working
checkout. The latter records selected-block hashes, adapters, and dependency-lock
identity. Final follow-up results are in `registration-final8-20261101.log` and
`registration-final8-20261101-receipt.json`. Both alias negative controls are
recorded separately; they are failures of intermediate candidates, not additional
executed baseline application tests. All five extracted production blocks match
the final candidate verbatim. The complete application build has not yet
produced a passing receipt; dependency compilation was killed under memory
pressure. Seven bounded attempts ended without reaching CASS, with unchanged
dependency pins, sources, and features.

The recorded component invocation, from the repository checkout with its
session harness and environment already prepared, was:

```sh
source ../cass-build-env.sh
CARGO_TARGET_DIR=../registration-probe-target cargo test --offline --locked \
  --manifest-path ../registration-probe/Cargo.toml --lib watch_registration_tests \
  -- --nocapture --test-threads=1
```

The new [library regressions](../../src/indexer/watch_ingestion_tests.rs) are:

- `gh538_watch_publishes_canonical_debt_after_source_disappears`: Codex and Claude,
  same-process and reopened handles, empty and changed-sibling retry; checks old
  identities, canonical rows, debt, receipts, and the following no-op scan.
- `gh538_watch_absent_source_recovery_preserves_debt_until_commit`: another
  publication failure plus an advisory-locked changed sibling.
- `gh538_watch_empty_absent_canonical_source_cannot_certify_publication`: retained
  debt, timestamp, and in-memory/persisted watermarks for an unproven empty replay.
- `gh538_watch_retries_disappeared_notification_leaf_without_another_event`:
  failure and retry through the actual dispatch queue using the same vanished
  leaf, without a new notification or process restart.
- `gh538_watch_missing_leaf_promotion_requires_current_matching_debt`: no-debt,
  connector, source-ID, path, removed-configuration, and deepest-root controls.
- `gh538_watch_narrowed_root_replays_only_its_share_of_ancestor_debt`: a retained
  missing leaf publishes only the currently configured child, preserves outside
  canonical work and ancestor debt, and completes only after the parent is
  explicitly restored as a current root.
- `gh538_watch_zero_emission_source_cannot_discharge_sibling_publication_debt`:
  Codex and Claude, empty and changed-sibling scans, actual retained dispatch
  queue, reopened storage, no publication on unchanged sibling retries, and
  eventual recovery without supplying another notification.

The new [subprocess regression](../../tests/watch_e2e.rs),
`deferred_watch_sources::gh538_live_watch_recovers_replaced_root_and_retains_history`,
requires a replacement conversation, successive appends, forced process
interruption, downtime and post-restart appends, six unique search sentinels,
two canonical conversations, and eight ordered messages. It retains the old
source and mirror. The existing
`deferred_watch_sources::gh538_live_watch_ingests_startup_append_and_restart_over_mirror_cap`
also remains required. Exactly-once assertions concern canonical rows and stable
searchable identities; recovery can execute more than once.

## Application commands still requiring receipts

Run on a host that can compile the pinned toolchain and dependency graph. A
focused diagnostic selection is:

```sh
export CARGO_BUILD_JOBS=1 RUST_MIN_STACK=134217728
cargo +nightly-2026-08-31 test --locked --lib indexer::watch_registration_tests -- --test-threads=1
cargo +nightly-2026-08-31 test --locked --lib indexer::watch_ingestion_tests -- --test-threads=1
cargo +nightly-2026-08-31 test --locked --test watch_e2e deferred_watch_sources::gh538 -- --test-threads=1
```

Run the repository gate without weakening its stages:

```sh
GATE_BUILD_JOBS=1 scripts/gate.sh --local \
  --lib-filter 'gh538 watch_lexical watch_reindex source_ledger' \
  --integration 'watch_e2e:deferred_watch_sources,cli_index:gh439'
```

Apply only the regression tests to the baseline for application negative
controls; retain baseline and candidate logs, source identities, actual failing
assertions, and positive terminal test counts. A dependency-build failure is not
a regression failure. Repeat the live tests natively on macOS under a shell and
an Interactive launchd job before attributing the reporter's platform behavior.
Any use of the reporter's archive requires separate access and authorization.

## Executed checks and remaining publication gate

The final candidate preserves the five native source blocks that passed the
component tests byte for byte. Pinned formatting and whitespace checks pass.
The machine-readable [validation receipt](issue-538-root-and-publication-validation.json)
records the final source identities and keeps native component results separate
from unexecuted application requirements.

The earlier frozen candidate's UBS v5.4.4 qualification completed a valid scan of
three changed Rust files with
49 critical, 10,905 warning, and 3,010 informational findings. Positive AST
results and content-matched cache records establish that source rules executed.
An earlier sandboxed run that reported zero findings without that evidence was
rejected as invalid. Of findings on added lines, one critical is the intentional
native regression timeout panic; 241 warnings include 225 test findings and
16 reviewed production findings. The production warnings concern necessary
owned-data clones, initialized writer access, indices with established bounds,
and error-context formatting. The other 48 critical snippets occur in the
unchanged baseline. Review found no additional production defect, but it does
not turn the scanner's failing exit code into a pass. No scanner exclusions or
suppressions were added. A four-file follow-up scan was rejected as invalid
because ast-grep was unavailable in that invocation's PATH. The corrected final
run used the verified ast-grep binary and completed with 49 critical, 11,025
warning, and 3,038 informational findings, exit 1. Fresh content-matched records
for all four inputs include 6,600 AST-only unwrap/expect findings and 150
direct-indexing findings. Its added-line inventory contains one critical
(the intentional native timeout panic), 347 warnings, and 67 informational
findings. The seven added warnings in native registration are owned-path clones
in bounded configured-root maps. The final scan is valid and failing; the
preceding low-count scan remains invalid. No gate policy or source suppression
was changed.

Seven meaningful application compilation attempts reached pinned
`asupersync 0.5.0`, never CASS. Three ended with actual OOM/SIGKILL and four were
cancelled to avoid exhausting the shared 8 GiB container while other builds
were active. The final guarded attempts assigned the expendable Cargo/rustc
children an OOM priority of 1000, left the receipt runner at its original
priority, and ran until an actual result. Neither completed. No other process
or shared resource limit was changed.

The attempts retained the exact dependency sources, versions, and features.
Environment-only trials reduced compiler concurrency, debug information,
incremental work, and Asupersync optimization; they also used immediate
allocator purging and one allocator arena. The final compiler completed type
checking and was killed during the following MIR analysis phase. None of these
trials reached the new canonical, live-watch, restart, or exactly-once tests.
There is no configured RCH/dsr build fleet or compatible complete CASS binary
available in this environment.

**Application tests executed: zero. The full repository gate has not passed.**
`AGENTS.md` requires the batched formatting, clippy, tests, goldens, and blocking
UBS gate before publication. These component results and source reviews are a
reviewable candidate, not evidence that the requested searchable-ingestion
qualification or the original macOS incident is resolved.

## Direct-main publication follow-up

After receiving the qualification limits above, the user explicitly renewed the
instruction to continue substantive work and commit directly to `main`, without
a pull request. This recovery change is published under that instruction. The
reported application and UBS results remain unchanged; publication does not
constitute a passing gate, a release qualification, or closure of GH538.
Additional application execution is being attempted in a newly available build
window. The same change includes the newly reproduced overlapping native-watch
correction and conservative zero-emission completion/retry handling described
above. The validation receipt distinguishes the earlier frozen candidate from
this follow-up; unexecuted application regressions remain unexecuted.

The follow-up full compilation completed Asupersync's borrow checking, then
exhausted memory during code generation. A subsequent metadata-only check was
also killed when another compiler entered the shared memory limit. The final
metadata-only retry has passed Asupersync and is continuing through the remaining
dependency graph. These attempts preserve the pinned sources and features;
none has executed the prepared CASS application regressions at this publication
checkpoint.
