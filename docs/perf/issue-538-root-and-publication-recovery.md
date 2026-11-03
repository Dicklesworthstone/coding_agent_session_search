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

## Continued resident recovery: November 3

Commit [`9995f13`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9995f13024f1f239b9cfe4027f412ea11f967294)
adds two corrections after reviewing current main, the October 31 evidence,
the comprehensive plan and the still-open
`coding_agent_session_search-gh538-watch-ingestion-20261031` Bead. It preserves
the intervening `1e68fdf` path-identity correction. Neither correction repeats
the existing source publication receipts, readiness or launchd renderer fixes.

### Discovered retries survive a later callback failure

A connector can discover an advisory-locked transcript and defer it before
admitting any canonical rows or publication debt. If a later operation fails,
such as reopening the archive after a storage recycle, the old callback
returned before draining that newly discovered retry. Retaining the original
notification is insufficient when it names a different, transient file that
subsequently disappears. The transcript can then become idle without another
write, leaving no event or durable publication debt to recover it.

The callback now drains its active, lexical and semantic retries after both
successful and failed work. A typed Anyhow context carries the exact paths
through the error; the dispatcher retains them alongside the original pending
paths. The original failure and cancellation type remain downcastable, and the
log preserves the complete cause chain. No synthetic roots are added, and the
existing source classification and authorization rules remain authoritative.
The existing bounded backlog still escalates overflow to a full census. A
failed full census retains that obligation without expanding the individual
path backlog or advancing a watermark.

The new real Codex/Claude regression starts without canonical rows, receipts
or replay debt. It discovers an advisory-locked transcript, induces a real
archive-path obstruction during storage reopening, removes the notification
leaf, unlocks the transcript without changing bytes or mtime, and requires
recovery through another pre-scan reopen failure. Its final assertions require
one published identity, no ingestion from an unrelated configured root, and
receipt reuse after storage restart. Execution of this complete fixture is a
separate requirement from the dispatcher component tests.

### Native errors rebuild recursive coverage

The pinned notify backend can lose a child's recursive enrollment while its
configured root retains the same path and inode. A full census discovers files
that already exist, but it does not restore notifications for subsequent
appends. Previously an error callback or rescan flag scheduled only that census;
the unchanged root identity could prevent native re-registration indefinitely.

Native errors and rescan flags now also retain a registration-recovery
obligation. At the existing registration cooldown, the watcher removes its
recorded native registrations and rebuilds them through the existing root
identity logic. The full-census obligation remains independent. Ordinary
notifications and application queue overflow leave healthy native registrations
in place. Cancellation checks, watchdog thresholds, source ownership and
FSEvents reconciliation after stream restarts are preserved.

### Executed evidence and its limits

The new tests were compiled from exact selected source blocks on Linux using
the pinned nightly and a locked subset whose 48 registry identities/checksums
match the application lockfile. The source hashes in the machine-readable
receipt match the published Rust blobs.

| Check | Previous logic | Published correction | Scope |
| --- | --- | --- | --- |
| Retain exact retries and original error identity | Expected assertion failure | Passed | Dispatcher/context component |
| Retain bounded backlog and full-census obligation | Expected assertion failure | Passed | Dispatcher/backlog component |
| Recover later appends after lost child enrollment | Actual notification timeout | Passed for error and rescan signals | Real Linux native watcher, injected callback boundary |
| Application overflow leaves healthy coverage enrolled | Passed | Passed | Real Linux native watcher |

The native test removes a real private child inotify watch, proves that a write
is missed, injects a notify error or rescan event, and then requires a real
subsequent append notification. It does not reproduce kernel watch-limit
exhaustion or mutate kernel limits. The baseline adapter only forwards the new
test entry point to the original registration function. The dispatcher
negative control supplies the new typed context producer to the original
dispatcher; it does not execute the old production callback or storage stack.

One initial assertion in the new backlog test incorrectly expected individual
paths after a failed full census. The existing implementation deliberately
retains only the full-rescan obligation in that case. The corrected test
requires the full flag, empty paths, zero retained bytes, and a subsequent
`force_full=true` callback. The initial failure was retained in the evidence;
no production behavior or deadline was changed to obtain the passing result.

Pinned Rust 2024 formatting and whitespace checks pass. Strict UBS v5.4.4
completed with **48 critical, 11,242 warning and 3,058 informational findings;
exit 1**. All 46 pre-existing critical sites map to unchanged baseline lines;
the two added critical sites are intentional native-test failure panics.
The 134 added-line warnings are test assertions and fixture operations. The
substantive production additions have no critical or warning findings; two
informational findings mistake tracing display syntax for modulo operations.
Content-matched cache records demonstrate actual AST execution. No suppression
was added. Cargo-backed scanner categories were explicitly not evaluated while
the separate full application build ran. This remains a failing gate.

A fresh `cargo test --offline --locked --test watch_e2e --no-run` attempt exited
101 after 1,530.52 seconds. The compiler for pinned Asupersync 0.5.0 was killed
with SIGKILL before CASS compiled; the shared 8 GiB cgroup's OOM-kill counter
increased during that phase. No application test executed. The attempt used
the unchanged dependency graph, one Cargo job and compiler thread, no debug
information or incremental compilation, and the normal dependency optimization
profile. No repeated full-code-generation attempt was used to mask that result.
The November 3 validation receipt records this separately from subsequent
metadata/Clippy checking. The all-target Clippy attempt also exited 101 after
597.52 seconds: its metadata-only Asupersync compiler was killed before CASS was
checked. Neither invocation produced a CASS source diagnostic, application test
result or full-source Clippy pass. Further heavy attempts were stopped.
Component results do not establish application execution,
native macOS behavior or recovery of the reporter's archive. In particular,
the pinned FSEvents implementation has silent stream-start failure paths that
produce neither an error callback nor a rescan flag; this notification-triggered
repair does not certify those paths. GH538 and its qualification Bead remain
open.

### Native macOS validation from current main

Use a native Mac able to compile the pinned, unchanged dependency graph. The
isolated application tests and the reporter's existing archive are separate
qualification stages. Preserve the source configuration, authorization rules,
retention policy, active-writer deferral and watchdog thresholds throughout.

#### Build identity and isolated tests

Run in a modern Bash; the repository gate's associative arrays require more
than macOS `/bin/bash` 3.2. The gate also expects GNU `coreutils` and `findutils`
ahead of the system utilities, plus `rg` and `ast-grep`. If installed with
Homebrew, start its Bash and add these tools to `PATH`:

```bash
set -uo pipefail
(( BASH_VERSINFO[0] >= 4 )) || exit 2
CASS538_BASH_PREFIX="$(brew --prefix bash)" || exit "$?"
CASS538_COREUTILS_PREFIX="$(brew --prefix coreutils)" || exit "$?"
CASS538_FINDUTILS_PREFIX="$(brew --prefix findutils)" || exit "$?"
export PATH="$CASS538_BASH_PREFIX/bin:$CASS538_COREUTILS_PREFIX/libexec/gnubin:$CASS538_FINDUTILS_PREFIX/libexec/gnubin:$PATH"
CASS538_RUN="$(mktemp -d "$HOME/cass538-native.XXXXXX")" || exit "$?"
git fetch origin main || exit "$?"
CASS538_SHA="$(git rev-parse FETCH_HEAD)" || exit "$?"
git worktree add --detach "$CASS538_RUN/source" "$CASS538_SHA" || exit "$?"
cd "$CASS538_RUN/source" || exit "$?"
CASS538_DIRTY="$(git status --porcelain)" || exit "$?"
test -z "$CASS538_DIRTY" || exit 1
export CARGO_TARGET_DIR="$CASS538_RUN/target"
export CARGO_BUILD_JOBS=1 RUST_MIN_STACK=134217728
git rev-parse HEAD > "$CASS538_RUN/source-commit.txt"
shasum -a 256 Cargo.lock rust-toolchain.toml > "$CASS538_RUN/build-inputs.sha256"
rustc +nightly-2026-08-31 --version --verbose > "$CASS538_RUN/rustc.txt"
sw_vers > "$CASS538_RUN/macos.txt"
uname -a >> "$CASS538_RUN/macos.txt"
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo +nightly-2026-08-31 build --locked --profile profiling --bin cass \
  > "$CASS538_RUN/build.log" 2>&1
build_rc=$?
printf '%s\n' "$build_rc" > "$CASS538_RUN/build.exit"
test "$build_rc" -eq 0 || exit "$build_rc"
export CASS538_BIN="$CARGO_TARGET_DIR/profiling/cass"
"$CASS538_BIN" --version > "$CASS538_RUN/version.txt"
"$CASS538_BIN" api-version --json > "$CASS538_RUN/api-version.json"
shasum -a 256 "$CASS538_BIN" > "$CASS538_RUN/binary.sha256"
```

The existing `profiling` profile inherits release optimization, LTO and panic
behavior while retaining symbols. Check that `build_commit` matches the checkout
without an unexpected `-dirty` suffix. Use this same absolute binary and hash
for the shell and launchd archive runs below.

Run these unchanged application selections and retain each command, exit code,
complete output and positive terminal test count:

```bash
cargo +nightly-2026-08-31 test --locked --lib \
  indexer::watch_registration_tests -- --nocapture --test-threads=1
cargo +nightly-2026-08-31 test --locked --lib \
  indexer::watch_ingestion_tests -- --nocapture --test-threads=1
cargo +nightly-2026-08-31 test --locked --lib \
  cli_read_db_tests::gh538 -- --nocapture --test-threads=1
cargo +nightly-2026-08-31 test --locked --lib \
  schedule::tests::gh538_launchd_jobs_keep_cpu_niceness_without_implicit_io_throttling \
  -- --exact --nocapture --test-threads=1
cargo +nightly-2026-08-31 test --locked --test watch_e2e \
  deferred_watch_sources:: -- --nocapture --test-threads=1
```

The last selection currently contains seven macOS tests, including all four
required live-watcher regressions:

- `gh538_live_watch_preserves_idle_mirror_and_recovers_repeated_updates`
- `gh538_live_watch_ingests_startup_append_and_restart_over_mirror_cap`
- `gh538_live_watch_recovers_replaced_root_and_retains_history`
- `gh538_live_watch_diagnoses_blocked_mirror_and_recovers_after_watchdog_abort`

The other three cover active-source, event and startup retries. The writable
descriptor test and the new missing-child-inotify-registration regression are
Linux-only. There is currently no automatic macOS counterpart to that injected
registration-loss test. The over-cap application regression requires 16 body
sentinels, seven canonical conversations, 23 messages and 18 lexical documents,
with preserved mirror evidence. The two readiness tests require outstanding
or malformed replay debt to override a recent publication/live lease, and a
failed debt probe to remain unknown instead of reporting ready.

Run the repository gate without skipping stages or increasing deadlines:

```bash
GATE_TARGET_DIR="$CARGO_TARGET_DIR" GATE_BUILD_JOBS=1 \
  bash scripts/gate.sh --local \
    --lib-filter 'gh538 watch_source_ledger source_ledger_tests raw_mirror schedule' \
    --integration 'watch_e2e:deferred_watch_sources,cli_index:gh439' \
  > "$CASS538_RUN/gate.log" 2>&1
gate_rc=$?
printf '%s\n' "$gate_rc" > "$CASS538_RUN/gate.exit"
```

Retain every `STAGE=... EXIT=...` and `TEST_COUNT=...` receipt. A compile failure,
zero-test selection, timeout, missing receipt or failing scanner remains a
failing qualification result.

#### Seven reporter-archive probes

Use the actual existing archive path confirmed by the custom plist and status
metadata. Without environment overrides the current macOS default is
`~/Library/Application Support/com.coding-agent-search.coding-agent-search`;
do not substitute a similarly named empty directory. Set the actual paths and
the original reproduction settings explicitly:

```bash
export CASS538_DATA="/absolute/path/to/the/existing/cass/data"
export CASS538_DB="$CASS538_DATA/agent_search.db"
export CASS538_ORIGINAL_PLIST="/absolute/path/to/com.cass.watch.plist"
test -f "$CASS538_DB" || exit 1
export CASS_AUTO_REFRESH=0
export CASS_CODEX_MAX_SOURCE_BYTES=734003200
export CASS_SKIP_PREFLIGHT_COUNT_TOTAL_MESSAGES=1
export CASS_SKIP_PREFLIGHT_FTS_SHADOW_VIABILITY=1
export CASS_TRACE_FILTER=warn,coding_agent_search::raw_mirror=trace
```

Those two preflight settings preserve the reporter's original workload; they
do not waive the independent repository gate or qualify those preflight checks.
Retain the disabled `cursor` connector, configured roots and exclusions; do not
set `CASS_IGNORE_SOURCES_CONFIG`. Preserve the original default mirror cap and
reserve, with no new `CASS_RAW_MIRROR_*` overrides. Keep watchdog detection/abort
at the original 120/300 seconds, the existing preflight deadline, and the
resident active-source recent-write default of 120 seconds. Do not disable
fsync, capture or source deferral to obtain a pass.

Arrange one index writer for the controlled run, recording any scheduled jobs
or other watchers suspended. Choose the same actual working directory for both
runs, including the location from which any `.env` and relative configuration
are resolved. Reconcile the shell with the original plist before launching:

```bash
export CASS538_CWD="/absolute/path/to/the/original/watch/working-directory"
export CASS538_COMMON="$CASS538_RUN/common-runtime.json"
cd "$CASS538_CWD" || exit "$?"
python3 - <<'PY'
import json, os, plistlib
from pathlib import Path
with open(os.environ['CASS538_ORIGINAL_PLIST'], 'rb') as source:
    original = plistlib.load(source)
controlled = {'PATH', 'CASS_AUTO_REFRESH', 'CASS_CODEX_MAX_SOURCE_BYTES',
              'CASS_SKIP_PREFLIGHT_COUNT_TOTAL_MESSAGES',
              'CASS_SKIP_PREFLIGHT_FTS_SHADOW_VIABILITY', 'CASS_TRACE_FILTER'}
conflicts = sorted(name for name, value in original.get('EnvironmentVariables', {}).items()
                   if name not in controlled and os.environ.get(name) != value)
if conflicts:
    raise SystemExit('Set the original plist values in the shell, then retry: '
                     + ', '.join(conflicts))
cwd = str(Path(os.environ['CASS538_CWD']).resolve(strict=True))
if 'WorkingDirectory' in original:
    if Path(original['WorkingDirectory']).resolve(strict=True) != Path(cwd):
        raise SystemExit('CASS538_CWD must match the original WorkingDirectory')
environment = dict(os.environ)
common = {'environment': environment, 'working_directory': cwd,
          'nice': original.get('Nice', 10), 'binary': os.environ['CASS538_BIN'],
          'database': os.environ['CASS538_DB'], 'data_dir': os.environ['CASS538_DATA']}
os.umask(0o077)
target = Path(os.environ['CASS538_COMMON'])
target.write_text(json.dumps(common, indent=2) + '\n')
target.chmod(0o600)
PY
common_rc=$?
test "$common_rc" -eq 0 || exit "$common_rc"
CASS538_NICE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["nice"])' "$CASS538_COMMON")" || exit "$?"
```

This stops on missing or conflicting original provider/policy settings, including
`CODEX_HOME`, XDG roots, exclusion settings and watchdog overrides. The explicitly
listed reproduction/trace settings and chosen `PATH` are common to both runs.
If the original job had no working directory, select and record the intended
one explicitly. Do not change runtime settings after making this snapshot.
Keep the common-runtime file and generated plist private: they contain the
runtime environment. Give each start its own evidence directory:

```bash
ATTEMPT="$CASS538_RUN/shell-1"
mkdir "$ATTEMPT"
nice -n "$CASS538_NICE" "$CASS538_BIN" \
  --db "$CASS538_DB" --trace-file "$ATTEMPT/trace.jsonl" \
  index --data-dir "$CASS538_DATA" --watch --watch-interval 30 --json \
  > "$ATTEMPT/stdout.jsonl" 2> "$ATTEMPT/stderr.jsonl" &
WATCH_PID=$!
printf '%s\n' "$WATCH_PID" > "$ATTEMPT/pid"
```

Keep a private seven-entry manifest containing a fresh single-word token,
source path/source ID, provider/session identity, actual write/release time and
expected logical message occurrences. Put each token in real message content,
and keep an old-content control phrase. Reproduce the original sequence:

| Probe | Operation |
| --- | --- |
| 1 | New conversation from a new project under `/tmp`, with its transcript under a configured provider root. |
| 2 | Append a new user message to that conversation. |
| 3 | Append another distinct message to the same conversation. |
| 4 | Stop the watcher with SIGTERM, append during downtime and restart the same binary with new log paths. |
| 5 | New conversation in an existing project directory. |
| 6 | New conversation in another new project directory. |
| 7 | New text in a session that was indexed before this run. |

For each token, save strict read-only search evidence:

```bash
CASS_AUTO_REFRESH=0 "$CASS538_BIN" --db "$CASS538_DB" \
  search "$TOKEN" --data-dir "$CASS538_DATA" \
  --mode lexical --json --no-maintenance --timeout 3000 \
  > "$ATTEMPT/$TOKEN.search.json" 2> "$ATTEMPT/$TOKEN.search.stderr"
search_rc=$?
printf '%s\n' "$search_rc" > "$ATTEMPT/$TOKEN.search.exit"
```

Predeclare a 300-second observation window after normal writer/deferral
conditions permit ingestion. Record latency and held-writer conditions; do not
change fixture deadlines or set the active-source window to zero. Use a fresh
filename for every polling observation. A search timeout is not evidence of
absence. Require the token in `hits[].content` with the expected source identity;
a title-only match does not pass. Check actual message occurrences and flag
unexpected duplicate identities instead of selecting one successful hit.

Before and after each probe, retain `status --json --robot-meta` and
`health --json --robot-meta` with explicit `--db`/`--data-dir` and exit codes.
Inspect `pending.lexical_replay`, `pending.watch_active`, `index.status`,
`index.fresh`, `index.partial`, publication time and canonical/lexical counts.
Liveness, advancing timestamps or `ready` alone cannot pass an ingestion probe.
Known debt must prevent ready/fresh status; a failed debt probe remains unknown.

For the shell restart, record the actual termination status:

```bash
kill -TERM "$WATCH_PID"
wait "$WATCH_PID"
watch_rc=$?
printf '%s\n' "$watch_rc" > "$ATTEMPT/watch.exit"
```

Write probe 4 before restarting. Do not run standalone `index`, `--watch-once`,
`search --refresh`, `doctor --fix`, or another catch-up before recording the
watcher's outcome. After all probes pass, observe several unchanged 300-second
quiet windows, checking old/new content, cap warnings and retained mirror
evidence.

#### Canonical identity and restart checks

Stop the sole writer and confirm its exit before copying the canonical database
and any `-wal`, `-shm` and `-journal` sidecars to a new private evidence directory.
Inspect that offline copy with SQLite URI `mode=ro` and `PRAGMA query_only=ON`;
do not let stock SQLite create or recover a WAL index beside the live archive.
Bind each affected source identity from the search evidence to:

```sql
SELECT c.id, c.source_id, c.agent_id, c.external_id, c.source_path,
       m.id, m.idx, m.role, m.content
FROM conversations AS c
JOIN messages AS m ON m.conversation_id = c.id
WHERE c.source_id = ? AND c.source_path = ?
ORDER BY c.id, m.idx;
```

Require each token's expected logical message count, one affected conversation
per `(source_id, agent_id, external_id)`, and one message per
`(conversation_id, idx)`. Preserve the existing ordered messages, including
legitimate repeated assistant text. Record canonical IDs, ordered content
hashes and counts, then restart only the same watcher without changing sources.
Repeat search and an offline snapshot check; the affected identities/order must
remain unchanged. Internal retries do not authorize duplicate canonical rows.

#### Resident Interactive launchd and standalone indexing

Generated `cass schedule` jobs are incremental/nightly scheduled runs. They
do not qualify a resident watcher or rewrite the user's `com.cass.watch` plist.
Create a separate controlled job from the original source environment and Nice
setting, using the same recorded binary and new evidence paths:

```bash
export ATTEMPT="$CASS538_RUN/launchd-1"
mkdir "$ATTEMPT"
export TEST_PLIST="$ATTEMPT/com.cass.gh538qualification.plist"
python3 - <<'PY'
import json, os, plistlib
from pathlib import Path
common = json.loads(Path(os.environ['CASS538_COMMON']).read_text())
attempt = Path(os.environ['ATTEMPT'])
job = {
    'Label': 'com.cass.gh538qualification',
    'ProcessType': 'Interactive',
    'ProgramArguments': [common['binary'], '--db', common['database'],
                         '--trace-file', str(attempt / 'trace.jsonl'), 'index',
                         '--data-dir', common['data_dir'], '--watch',
                         '--watch-interval', '30', '--json'],
    'EnvironmentVariables': common['environment'],
    'WorkingDirectory': common['working_directory'],
    'Nice': common['nice'],
    'RunAtLoad': True,
    'KeepAlive': False,
    'StandardOutPath': str(attempt / 'stdout.jsonl'),
    'StandardErrorPath': str(attempt / 'stderr.jsonl'),
}
os.umask(0o077)
with open(os.environ['TEST_PLIST'], 'wb') as output:
    plistlib.dump(job, output)
PY
plutil -lint "$TEST_PLIST"
launchctl bootstrap "gui/$(id -u)" "$TEST_PLIST"
launchctl print "gui/$(id -u)/com.cass.gh538qualification" \
  > "$ATTEMPT/launchctl.txt"
```

The generated plist contains absolute strings; launchd does not expand shell
variables or `~`. Its environment and working directory come from the same
recorded shell runtime. `KeepAlive=false` prevents the controlled job from hiding an
unexpected exit behind automatic restart. Retain the plist, loaded job details,
actual PID, binary hash and last exit status. Repeat all seven probes, restart,
quiet-window and canonical checks with fresh tokens. Stop the controlled job
with `launchctl bootout "gui/$(id -u)/com.cass.gh538qualification"` before another
writer starts. Separately inspect the production custom plist for its actual
`ProcessType=Interactive`, current absolute binary and `--watch` arguments.

Only after freezing the watcher outcomes, qualify the independent standalone
indexing case. With the watcher stopped, write a fresh, separate standalone-only
input and body token. Verify that token is absent through the strict read-only
search before indexing; retain that observation and normal writer/deferral
conditions. Create another private job with a distinct label/output directory
and the same binary, environment, Nice and Interactive settings. Its arguments
are `--db <actual-db> --trace-file <new-trace> index --data-dir <actual-data> --json`,
without the resident watch flags. Require an actual completed run and exit 0,
the new token searchable in its expected body/source, and its expected canonical
message count without duplicate identities. Retain counts, timing and logs;
an empty/no-op completion is insufficient. If it discovers content missed by the
watcher, retain that discrepancy as a watcher failure; a successful standalone
run does not replace the earlier missing-content observation.

#### Failure evidence

While a stalled process still exists, retain native diagnostics without changing
the watchdog thresholds:

```bash
/usr/bin/sample "$WATCH_PID" 5 1 -file "$ATTEMPT/sample.txt"
/bin/ps -p "$WATCH_PID" -o pid,ppid,state,%cpu,etime,command \
  > "$ATTEMPT/process.txt"
```

Use the actual launchd PID for a launchd attempt. Preserve the structured stall
event, compact `index-stalled` envelope and exit 70 when emitted. Inspect
`diagnostics.raw_mirror_io` for operation/path/elapsed time;
`diagnostics.filesystem_probe.status="pending"` means optional inspection did
not finish. Keep complete stdout/stderr/trace, status/health, search bodies,
canonical checks, timestamps and effective settings for each attempt.

Source/component tests, native application fixtures and reporter-archive runs
are separate receipts. A passing reporter rerun establishes behavior for its
recorded machine, build, archive and settings; it does not establish the
original blocked syscall/path or fully explain the launchd I/O correlation.
