# GH538: root registration and publication recovery

Investigation date: 2026-10-31. Baseline:
[`5cfb9d6a033927255a8a4befad153a44c8ed36e4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5cfb9d6a033927255a8a4befad153a44c8ed36e4).
The native registration and missing-child publication corrections were committed
directly to `main` as
[`52e5e9d4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/52e5e9d4125cb08ac5aaab2f75d8ae460a8a7e96).
The containing follow-up additionally covers a wholly absent current root.

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
generation exceeded the available memory**. The final frozen-source
`cargo check --all-targets` has passed against the pinned graph, including the
library tests and `watch_e2e` target. The same frozen source also passed
`cargo clippy --all-targets --no-deps -- -D warnings`. These checks do not
execute tests.
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

The follow-up also corrects the polling backend's initial-error contract. In
pinned notify 8.2.0, `PollWatcher::watch` returns `Ok` even when its synchronous
callback reports failure to initialize the watched directory. A real missing
parent reproduction on `52e5e9d4` therefore left registration retry disabled.
The poller now captures initial callback errors for its selected parent paths
and folds them into the retry result. Errors for unrelated children remain
diagnostic, and the error callback does not request a global rescan.

Every poller rebuild attempt also queues ordinary reconciliation of its current
database roots. A new polling snapshot cannot report writes made during the
registration gap. This includes partial registration failures: repeatedly
rebuilding a healthy peer alongside a failing directory must not strand that
peer's WAL updates. The scan uses existing idempotent database ingestion and
does not force a scan of unrelated conversation roots.

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

### Canonical writes whose whole configured root disappears

The same obligation can outlive its entire root directory. Previously, both the
retained notification leaf and the current root failed classification, so saved
canonical messages remained unpublished until the directory returned.

Missing-path routing now separates eligible absent roots from ordinary source
scans. It requires a current connector, source origin, and configured root that
intersect non-discovery publication debt. Only a filesystem `NotFound` result
certifies root absence; permission errors, lookup loops, and non-directory paths
cannot authorize this route. When an eligible extant owner exists, its ordinary
census retains precedence. Duplicate and nested absent selections coalesce only
within the same currently authorized family.

The canonical-only route uses the existing bounded replay to publish already
admitted messages. It never detects or scans a connector, creates a source
receipt, changes `last_indexed_at`, or advances in-memory or persisted watch
watermarks. Publication must commit before its debt can clear. A final absence
check withholds clearance and queues ordinary reconciliation if the root has
returned. A root that disappears between classification and ordinary detection
likewise retains its retry rather than permitting a connector's default-path
fallback.

Current path, workspace, subagent, and deferred-lexical policies preserve debt
without scheduling an intentional-policy retry loop. Matching unexpired NoMem
deferrals retain cooldown retry. Active-source exclusions remain in force.
Discovery-only debt, an ancestor obligation extending beyond current roots, and
empty canonical identities remain unproven. Old debt alone never restores a
removed source configuration.

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

The following limits remain explicit:

- A wholly absent root can publish admitted canonical messages only while that
  root remains authorized by current configuration. Source discovery and new raw
  ingestion still require the directory to return.
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
| Polling callback registration failure retains retry and reconciles peers | Published `52e5e9d4` failed retry assertion | Passed | Native component |
| Canonical recovery and actual pending-leaf retry | Regression tests prepared | Pending execution | Full application library |
| Live replacement, over-cap updates, and restart | Regression tests prepared | Pending execution | CASS subprocess and search |

The baseline pair, `gh538_native_watch_retries_failed_root_registration` and
`gh538_native_watch_rearms_replaced_root`, failed **2/2** in 30.03 seconds. The
initial component selection passed **5/5** in 21.51 seconds. The first published
follow-up passed **8/8** in 20.08 seconds. The final selection, including the
polling registration regression, passed **9/9** in 27.04 seconds. Its negative
control on the published `52e5e9d4` polling implementation failed the intended
retry assertion, with **0 passed and 1 failed**. These times cover test fixtures and
waits, not indexing throughput. Session evidence is retained in `registration-baseline.log`,
`registration-final.log`, and `registration-probe/sources.json` beside the working
checkout. The latter records selected-block hashes, adapters, and dependency-lock
identity. Final results are in `registration-final9-20261101.log` and
`registration-final9-20261101-receipt.json`; the polling negative is in
`registration-poller-negative-20261101.log`. The earlier eight-test receipt
remains a separate checkpoint. Both alias negative controls are
recorded separately; they are failures of intermediate candidates, not additional
executed baseline application tests. All five extracted production blocks match
the final candidate verbatim. The executable application build has not produced
a passing receipt; dependency code generation was killed under memory pressure.
Seven initial bounded attempts ended without reaching CASS. Later attempts
reached the actual application metadata checks, found and corrected six test
compilation errors, and passed a final all-target check. Dependency pins,
sources, and features remained unchanged.

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

The whole-root follow-up adds nine application regressions:

- `gh538_watch_publishes_canonical_debt_after_whole_current_root_disappears`:
  Codex and Claude, retained-leaf retry and reopened handles; preserved rows,
  historical search identities, receipts, timestamps, and watch state; a repeated
  callback publishes nothing after debt clears.
- `gh538_watch_absent_root_recovery_requires_commit_before_debt_clearance`:
  another commit fault retains the queue, debt, and previous searchable state.
- `gh538_watch_absent_root_recovery_requires_current_authorization`: removed
  configuration, provider/source mismatch, and unchanged explicit watch-once.
- `gh538_watch_absent_root_routing_prefers_extant_owner_and_coalesces_duplicates`:
  existing ancestor ownership and one replay per canonical identity.
- `gh538_watch_absent_root_selection_rejects_nonabsence_errors` and
  `gh538_watch_absent_root_lookup_loop_cannot_authorize_recovery`: typed error
  policy and a real Unix filesystem lookup loop. The permission control is not a
  claim of an unprivileged filesystem permission-denial run.
- `gh538_watch_absent_root_preserves_nomem_retry_and_discovery_debt`: backoff
  expiry permits publication while discovery debt survives the missing census.
- `gh538_watch_absent_root_empty_canonical_identity_retains_debt`: an empty
  canonical conversation cannot prove removal of stale lexical results.
- `gh538_watch_absent_root_honors_exclusion_skip_and_defer_policies`: isolated
  child processes verify each current policy without racing process environment.

The tests-only `gh538-whole-root-negative.patch` applies to `52e5e9d4` without
production changes and adds the primary whole-root scenario plus the required
test compilation corrections. The expected
baseline failure is zero recovered conversations where two are required. Patch
applicability was checked; this application negative control has not executed.

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

The final nine-test component run extracts five production source blocks from
the corrected candidate verbatim. Their hashes and all 53 resolved registry
dependency identities/checksums were independently checked. Pinned formatting
and whitespace checks pass.
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

The containing follow-up has its own final two-file scan after the whole-root,
polling, and test compilation corrections: **46 critical, 10,950 warning, and
2,986 informational findings; exit 1**. All 46 critical findings are outside the
newly added lines. Added lines contain 197 warnings and 38 informational
findings. The added production findings include 12 warnings: seven bounded
configured-root clones, two indexes with local construction invariants, and
three error-context closures. Review found no concrete additional production
defect. Fresh content-matched cache records contain 6,508 AST-only unwrap/expect
and 150 direct-indexing findings. The scan is valid and remains failing, with no
new suppression or scanner-policy change. It covers exactly the two Rust files
changed since `52e5e9d4`; the earlier four-file result remains a separate
checkpoint.

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

The final all-target metadata check completed with exit 0 in 626.10 seconds,
with no new OOM. All four recorded Rust source hashes and all four dependency,
toolchain, and Cargo configuration hashes matched before and after the run.
This includes the production library, complete library test module, live-watch
integration target, and remaining integration/benchmark targets. The only
terminal warning concerned the pinned transitive `nix 0.28.0` dependency's
future Rust compatibility. It was not a CASS source diagnostic.

The final same-graph Clippy run also completed with exit 0 in 729.70 seconds,
using `--all-targets --no-deps -- -D warnings`. Production code, the complete
library test module, `watch_e2e`, and the remaining targets passed with denied
warnings. Source and pin/configuration hashes again matched before and after,
with no dependency rebuild or OOM. The same Cargo dependency-compatibility
notice remained. Neither successful check executes an application test.

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
The first direct commit includes the reproduced overlapping native-watch
correction and conservative zero-emission completion/retry handling. The
containing follow-up adds whole-root canonical publication and polling
callback-error/snapshot-gap recovery. The validation receipt distinguishes the
earlier candidate, the first published commit, and the final source; unexecuted
application regressions remain unexecuted.

The follow-up full compilation completed Asupersync's borrow checking, then
exhausted memory during code generation. A subsequent metadata-only check was
also killed when another compiler entered the shared memory limit. The final
metadata-only retry passed Asupersync and reached CASS, where it exposed two
untyped boolean callback parameters and four diagnostics from an old polling
test's argument/return assumptions. These test-only compilation defects were
fixed in place. A subsequent frozen-source all-target metadata check passed.
The required same-graph Clippy check is recorded separately in the validation
receipt. These attempts preserve the pinned sources and features; none has
executed the prepared CASS application regressions.
