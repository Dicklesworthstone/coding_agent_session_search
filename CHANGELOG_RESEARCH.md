# CASS 0.8.0 changelog research

Next-release preparation (September 12 UTC, unreleased, bead yrjna): local
Shelley/Grok Bot integration is represented by commits `5d187f87`, `59b5d10c`
and the native-ID changes after v0.8.0. The published FAD dependency is now
0.2.4; historical 0.2.3 references below identify the discovery evidence used
at that time. The source-backup repair in `ee64d3b1` replaces an overwriting
copy with exclusive destination creation and bounded retries. It adds real
file and symlink conservation tests; those tests have not yet executed.
Crossbeam-channel 0.5.17 is committed as `f5328832`. Its first remote gate
was interrupted before a compiler or test verdict; the unchanged-source
continuation passed formatting, all-target Clippy and 35 focused tests. Its
strict UBS Rust module timed out after 300 seconds; source-after verification
passed. Neither that update nor the planned 0.9.0 release has full release
clearance. Asupersync 0.4.11 and its four companion packages are the next
isolated dependency candidate; validation is pending.

Fleet follow-up (September 11, unreleased, bead av59c): published FAD 0.2.3
lists `~/.config/muse/auth.json` and `~/.config/muse` as local discovery probes.
CASS previously admitted both into automatic remote source configuration.
The shared discovery/configuration predicate now excludes that configuration
tree, including old reports classified as unknown, while retaining Muse's
data roots. Existing manually configured sources are not rewritten. Focused
remote validation passed as recorded below; this finding does not establish that any
operator credentials were actually transferred.

The same fleet review found `run_setup` cleared its saved state before the
CLI attempted final sync, contradicting the failed-sync resume hint. State
is now retained while sync is pending. The new CLI regression starts from
fixture-backed completed setup, exercises a real OpenSSH transport failure,
and checks repeated JSON resume still reports pending sync with the selected
host and source configuration preserved. This is not successful live SSH
recovery or all-ten-machine acceptance.

Remote validation completed on September 11 at 20:26 UTC. Formatting and
all-target Clippy passed, along with 34 library tests and the CLI regression.
A test-only follow-up explicitly bound `CASS_DATA_DIR` and improved failure
diagnostics; its formatting, scoped Clippy and CLI rerun passed. The first
run's environment was checked and had no inherited data-directory override.
Strict UBS 5.3.13 remains red: the four-file scan reported 5 critical findings
and 1,781 warnings; the final test-file scan reported zero critical findings
and 1,108 warnings. No findings were suppressed. The three production files
and final test match their tested hashes. This is local unreleased work,
not full-suite, successful fleet-recovery or release acceptance.

Subsequent validation (2026-09-11 UTC, unreleased): v7 passed formatting,
all-target Clippy, 178 library tests, 68 integration tests and 68 goldens.
Two integration failures remain recorded: a Devin WAL append returned four
instead of five messages, and the rebuilding-generation CLI test took its
lock before its copied legacy fixture had a usable Quill generation. The
source-resume SIGINT, SIGTERM and bounded-stop journeys all passed. Strict
UBS remained red: the Rust module exceeded 300 seconds and Python reported
one warning. Frozen development CLI SHA256:
`802aa06f83454f1aeba732aff74f32309ab390711cbc579fe1d8ec0f6820ffc3`.
The next frozen candidate includes the Devin whole-second cutoff correction,
real fixture admission before the rebuild lock, one-pass conversation
statistics, and explicit pending-analytics completion with maintenance
exclusion. Its selected runtime checks are recorded below. A later comment-only
Python change documents a narrow UBS 5.3.13 false positive: `json.loads` already has an
enclosing `JSONDecodeError` handler. It is outside that frozen candidate and
passed a separate strict pinned scan: one file, zero critical findings, zero
warnings, and 49 informational findings. Sixteen JSON/error-path controls also
passed. The same-line pragma applies to every finding on that line, not one
rule. It does not resolve the Rust scanner timeout.

The v8c gate finished on September 11 at 08:09 UTC: formatting and all-target
Clippy passed, with 225 library, 86 integration and 68 golden passes. One OMP
integration failed and one semantic test remained ignored. The new Devin
same-second WAL append/replay test passed, as did all four deferred-watch
journeys, five Shelley tests and three interrupted-resume journeys. The OMP
failure occurred in a new test helper that parsed empty stdout after an
expected failure; production fatal robot JSON is emitted on stderr. That
helper also lost the child's stderr, so the original exact envelope cannot
be recovered. A test-only correction preserves all existing assertions and
adds exact error-code/stream checks. Its separate focused remote rerun passed:
one test, zero failures, 11.00 seconds, with formatting and scoped Clippy also
passing. The actual journey covers deferred search, full analytics completion,
replay conservation, missing-marker refusal and all five held-lock conflicts.
All other 3,751 frozen inputs, the original private Git HEAD, the broad-run
receipts and the application binary stayed unchanged. The formatter-only
patch was reviewed and applied to the canonical test; its SHA256 is
`d42edfbd1bb44f6f67509c06acb135f0e80e07501ae3cfe63c7b6bf0f07a5051`.
Focused receipt SHA256:
`f51bc7bb92563b1f574779024377f939be13f0e4ee9c4592ffe33a0c758f2135`.
The original frozen source stayed unchanged through the broad run. Receipt
SHA256: `d9552cf8bd1bf7b31766b141594513c19212c7fd575cc8ebc1a9c149117014ef`.
The full gate remains red: strict UBS timed out its Rust module at 300 seconds
and reported the Python warning predating the separate comment-only fix.

The v8c development executable
`1182c93d1ba643a1988cb628796d6e33a29c2fed6fd81ca16d5f9af4f75d4241`
was run once on the retained current archive copy. Statistics preserved all
13,990 conversations, 2,410,329 messages, dates, agent counts and ordered
workspace results. Elapsed time was 10.25 seconds and maximum RSS was
1,498,792 KiB: no clear improvement over the retained earlier development
run. Intervening analytics work changed the archive's physical state between
runs, so this is not an identical-state performance comparison. The archive
observations and executable stayed unchanged within this run, and the whole
process group stopped without a guard intervention. No additional statistics
run is warranted until a relevant implementation change is available.

Original issue validation update (2026-09-11 UTC, unreleased): reviewed the
source changes recorded in `ed7d166a` and the complete v6 remote gate receipt.
The fixed snapshot passed formatting, all-target Clippy with warnings denied,
175 selected library tests, 53 selected integration tests and 68 goldens.
Two integration tests failed: Shelley batch metadata retained an old analytics
workspace, and the 80-source signal fixture's prefix query matched neighboring
source IDs. Both repairs and subsequent fallback-cancellation, progress and
stale-cursor corrections await v7 validation. Strict UBS remains red because
the pinned Rust scan times out and the Python scan reports findings; this is
not release clearance. Frozen CLI SHA256:
`9424674a31c3e0d28af3c4ad573d38ab2e2f32bc10762b4f122456706d9fc1bb`.
The registry-backed source-resume test passed in both indexing modes. Bounded
analytics-reset interruption/reopen and absent-FTS Codebuff storage controls
also passed; the latter does not establish published Codebuff connector
adoption. Current archive validation remains separate from original reporter
archives: a retained 2,410,329-message copy resumed analytics through 595,664
messages, then stopped with typed interruption/143 without forced termination.
Its measured peak was 3,835,012 KiB. These corpus probes used development
binaries from the test gate, not optimized release builds, and do not certify
released performance. Old/new stats outputs agreed, but the approximately
10-second runtime and 1.55 GB memory use did not improve in those runs.
Private archives and machine identities are kept outside the repository.
Separately, the official v0.8.0 Linux x86_64 asset was checked against its
published archive checksum and executed once per search mode on the retained
current copy. Both lexical and default-hybrid searches timed out during setup
at the requested 3,000 ms and returned zero hits despite exit 0. Neither run
demonstrates working search on that archive. The executable and archive
observations were unchanged and both process groups stopped. This official
release baseline uses different source and optimization settings from the
development probes above; the timings are not an old/new performance comparison.
A separate single invocation using the official release's unmodified default
120,000 ms budget succeeded: five distinct hits, no timeout, 6.94 seconds and
2,558,324 KiB maximum RSS. Hybrid intent fell back to lexical because model
download consent was absent; no model was downloaded. Source archive and
executable observations were unchanged, and all child processes stopped.
This establishes default-command retrieval on this current copy. It does not
satisfy the earlier three-second probes, memory targets, semantic retrieval,
or original-reporter acceptance. The earlier short-budget failures must not
be read as proof that default search cannot finish.
GitHub metadata still identifies v0.8.0 as the published 2026-09-10 release;
these subsequent source changes are unreleased. Original issue acceptance
remains open.

Original issue implementation follow-up (2026-09-11 UTC, unreleased): Prime
explicit-file routing, migration-aware schema-only storage admission, known
legacy hash-space rebuilding, and completed-backfill no-op caching are under
combined remote validation. The cache is a maintenance skip hint, never search
serving authority; Unix descriptor identity and DB/WAL/vector change stamps
bind it to the completed artifact. Changed archives still require canonical
reconciliation, and concurrent indexing is still excluded. New tests include
same-tail edits, symlink WAL routing, old read transactions, replaced archive
paths, legacy wrong-vector replacement, and real Prime watcher notifications.
No native large-archive performance or remaining issue closure is claimed.
The fleet-only follow-up gate below timed out after1800s during Clippy and
executed no Rust tests; the combined candidate uses the supported10800s RCH
transport limit without changing behavioral assertions or scanner policy.

Reingest follow-up (2026-09-11 UTC, original `av59c`, unreleased): reviewed the
source after `b65e6803` and the retained two-mirror reproducer below. The source
filter previously narrowed only the report. The new scan entry point selects
remote roots, suppresses local discovery and historical salvage, forces mirror
recovery past unchanged-file fingerprints, and preserves global/per-connector
local watermarks in both streaming and batch ingestion. Mixed valid/unknown
filters fail before archive creation; preview discovery no longer opens the
database before the index lock. Both configured and database-fallback mirror
discovery now hash the exact configured path, matching sync for a bare `~`.
Real CLI regressions cover selective/full/replay/all-mirror ingestion, missing
mirrors, watermark preservation and subsequent local indexing; execution is
pending. A separate synthetic harness reproduction proved the TMPDIR symlink
privacy bypass: one inventory was written inside an isolated fake checkout
before the fix; after resolving the artifact root, the same case exits 2 with
no inventory inside, while normal external initialization still succeeds.
The first remote validation snapshot omitted `.gitattributes`, so all-target
Clippy failed at its existing `include_str!`; that attempt is retained and
cannot certify the gate. Final validation and the unchanged all-ten-host
authentication/strict-UBS acceptance remain pending. GitHub metadata was checked:
v0.8.0 is published at 2026-09-10 16:11:54 UTC; these fixes are unreleased.

Bounded setup probe follow-up (2026-09-10, original `av59c`): commit `7459cdeb`
limits optional health/stats/du/find measurements to a shared two-second budget.
Directory presence remains useful when measurements are unavailable; estimates
remain null rather than becoming false zeros. `InstalledUnknown` retains the
installed version, remains selectable for sync, and does not request indexing
solely because the optional check failed. A real ten-machine baseline at the
same 10-second deadline found eight reachable; both Mac probes timed out.
The remote gate passed formatting, all-target Clippy and 146 tests (76 library,
two setup CLI, 68 unchanged goldens; zero failed or ignored). UBS completed with
three critical and 320 warning findings, so `STAGE=ubs EXIT=1` remains red.
The three critical findings are pre-existing: two panics in the test-fixture
loader and an integer session-count comparison misidentified as a secret
comparison. The warning inventory is not waived or claimed fully resolved.
All 1040 CASS and 77 FAD inputs were unchanged after the gate, and the three
canonical files match its manifest. Gate log SHA256:
`665898d6e40e200dd8fb5cc09b2ec9d69c95150961466e35b75b453698661b08`.
The matched live comparison improved reachability from eight to nine of the
same ten machines at the same 10-second deadline. The previously failing newer
Mac completed in 2548 ms and exposed 27 session roots. Every previously reachable
host remained reachable and retained all previously detected paths. The older
Mac still timed out; the CLI's exit 0 for partial setup is not an all-ten pass.
Executable SHA256: `8a7b8db232cddd048733cba950ba4f72fe1d04f387b7a3eeb943d4267cae3197`.
Private ordinal summary SHA256: `7ac5fa4b436ec022ea51203f9b1c214e6572925321c4c98b3f893623dd2ce861`.
This is setup-probe validation; the full sync/search lifecycle was tested in the
preceding fleet run below, not repeated for this executable.
Separately, an isolated two-mirror CLI reproducer confirms that
`sources reingest --source alpha` also ingests beta while reporting only alpha;
the original bead retains that unresolved scope defect.
The gate's first
payload failed before compilation because a concurrent commit made the exported
HEAD diff empty; the replacement patch uses verified baseline hashes. No result
is credited to that failed attempt. GitHub metadata reports v0.8.0 published at
16:11:54 UTC targeting `96510ff5`, which contains the runtime change; this later
verification is not evidence that its published binaries passed this gate.

Fresh-eye fleet review (2026-09-10, `av59c`): the previous executable reproduced
three discovery defects with isolated generic configuration: `Match` overwrote
the preceding host's address, a reused source label falsely marked a different
target configured, and an existing target under a different label was not skipped.
The patch fixes these, quoted/commented aliases, first-value precedence, the
invalid printed add command, and zero-transfer `will_reindex` metadata. It also
bounds child output during collection and keeps timeout/error cleanup, rather
than checking size only after allocating the entire Tailscale response.

The final schema pass additionally found that JSON `null` for an empty Tailscale
address slice rejected the entire provider response. The amended parser treats
null/missing addresses as empty and skips that peer; the existing mixed-peer test
now covers this case. The final amendment passed formatting, all-target Clippy,
and all 91 selected tests, with no failures or ignored tests. The rebuilt binary
repeated the complete live workflow successfully on nine authenticated machines;
the tenth still requires authentication, so the overall harness exits 1.
The final gate ended at 02:45 UTC with `STAGE=ubs EXIT=1` after a 300-second
`MODULE_TIMEOUT`. All 1040 CASS and 77 FAD source inputs were unchanged after
execution; the four reviewed Rust files match the build manifest and the live
binary matches the executable receipt. This does not clear the release gate.
Final executable SHA256: `7241298dc93353fe3aef6456c092bef5ac87c36831e43a84b9c6f990551268d4`.
Final private ordinal summary SHA256: `0d3f4478760e1a4d8825196c952772e48ba483391bc9a441df1eed02234b5cdd`.
Final gate log SHA256: `fb3b916cc07a1e12ad36b73102b509a7b7a409782e7232f3e74ba974a4193b43`.
The following comparison receipts describe the preceding review build, retained
separately from this final amendment.

The harness now rejects Python `-O` (observed exit 2), avoids private-path
tracebacks for invalid input (observed exit 2), and compares content and host
provenance across global, source-scoped and default-hybrid results. Formatting,
all-target Clippy and 91 tests passed (nine library, six sources CLI, two setup,
six index JSON, 68 unchanged goldens). Three same-invocation before/after CLI
comparisons confirm the discovery defects fail on the previous binary and pass
on this one. Three additional malformed/private-input cases exit 2 without
tracebacks or path leakage.

The strengthened live harness passed all workflow checks on nine authenticated
machines: initial/replay/busy counts 18, mirror recovery 27, appended/offline
counts 36. Content and host provenance agree across all query scopes. A zero-file
replay now reports `will_reindex=false` (previous binary reported true). The tenth
host still requires authentication, so the overall live result is failed. That
gate ended with a 300-second UBS timeout and unchanged source inputs; this is
not release clearance.
Reviewed executable SHA256: `2a861819bb08f8caf51496a67a31b5fdb9a688736028c16e4e192496b806cd0a`.
Reviewed harness SHA256: `9aec231f6d19fd24f07df46471c7f5b65b6a57c5ebd32e8eeca406c9e4526dc4`.
Private ordinal summary SHA256: `7f27406adf5966f6b7ca0fa3c0786445ce82c4222ca488d5fa5a322ee7264c97`.

Optional Tailscale discovery (2026-09-09, original bead `av59c`): owner-requested
`--tailscale` is wired through both discovery and setup. Local status is bounded
to five seconds; online peer IPv4 addresses merge with configured SSH aliases,
without changing authentication or host-key policy. Offline peers, the local
node and IPv6-only peers are omitted. The new frozen remote gate passed formatting,
all-target Clippy and 88 tests: six library, six sources CLI, two setup-option,
six index JSON, and 68 unchanged goldens. The missing-executable CLI test verifies
that optional discovery failure retains SSH aliases and reports a warning.

The new executable then ran the actual Tailscale lane of the private fleet harness.
Discovery added 15 online peer candidates to the two explicit test configuration
entries; all requested peer addresses were found without being declared as SSH
aliases. Nine authenticated machines passed initial sync/search (18 messages),
unchanged replay, index-lock refusal (exit 7), mirror recovery (27 messages), a
second append (36 messages), and refused-source partial failure (exit 8, existing
36 messages preserved). Exact source/host provenance, negative source filters and
default-hybrid lexical fallback passed each phase. One requested machine still
requires human authentication, so the ten-machine harness correctly exits 1.

Executable SHA256: `a28c5028842655c86c8d79f23dc0edb8c7a9d9a3f014fb26c6afe6c6feb74a14`.
Harness SHA256: `736800dd4b73d8fd679ffb97b4de02b7dc5c58059bdd25ab4e31ff7d1322b655`.
Private ordinal summary SHA256: `fd65284b3367cfad0f965e7f228fadb9aa0013073d3f65103a94fbb46f52091f`.
The gate finished at 23:06 UTC: UBS timed out after 300 seconds (`MODULE_TIMEOUT`),
so `STAGE=ubs EXIT=1` keeps the release gate red. All other stages passed. The
post-run check verified all 1040 CASS and 77 FAD inputs unchanged, and the final
binary matches the live-run SHA above. Gate log SHA256:
`2ec0efa5276467e29b8d3c09b6a17919ba27a7de63cfcea469fc4ce3905d07b1`.
This is neither an all-ten pass nor a release clearance. Source snapshot also retains a concurrent answer-pack
command change to always include its database argument; it is not a Tailscale fix.

Fleet investigation (2026-09-09, bead `av59c`): live SSH testing exposed two
source defects. Discovery ignored the SSH configuration override used by
transport and did not read Include files. Sync/reingest printed the nested
indexing result as a separate JSON document. The fixes use the same configuration
for discovery, enumerate included aliases with bounded recursion, and capture
indexing output inside one final response with truthful failure status.

The frozen remote gate passed formatting, all-target Clippy, six library tests,
six sources CLI tests, six ordinary index JSON tests, and 68 goldens (86 total).
The new CLI cases exercise real configuration files, an actual held indexing
lock, real mirror ingestion, and a source-filtered search. The live SSH harness
then ran with all ten entries in an external private inventory: nine machines
passed, and one required human authentication. The overall result is failed,
not an all-ten pass. No machine identities or raw receipts are included here.

On the nine reachable machines: initial ingestion returned 18 hits; replay kept
the same 18 identities; a busy-index sync returned exit 7 while search retained
18; mirror recovery returned 27; a second append returned 36; a genuinely refused
SSH connection produced exit 8/partial while all 36 remained searchable. Exact
origin-host/source provenance and local/unknown-source negatives passed. Default
hybrid matched lexical results without a model download; this does not validate
neural semantic retrieval or archive-scale performance. Every sync/reingest
response parsed as one JSON document. Existing user archives were untouched.

Executable SHA256:
`8e30a55278a9823f7cd292bf60d45e8e949f2a14eb713acf5d0c86ff4973d822`.
Live harness SHA256:
`467e7d340013f81815749aa2da887e709d12f70eece6a33aa55939b49688f36c`.
Ordinal summary SHA256:
`87d69215795f6ab8d3d52d843a5e64d121a6951c80ae3b18acce8daa61ee325a`.
The initial harness missed the two-document bug because it checked exit codes
and search results; strict parsing was added after inspecting that retained
output. An earlier installed 0.7.1 run also rejected `sources sync --all`, an
already-fixed unreleased CLI incompatibility rather than a new repair here.
Setup still drops slow deep probes: the default timeout selected six of nine
SSH-reachable machines, and 30 seconds selected eight. That separate problem,
the authentication-blocked host, and strict release gates remain open.

The fleet gate ended at 22:26 UTC with UBS `MODULE_TIMEOUT` after 300 seconds,
zero completed files, and exit 1. All 1040 frozen CASS inputs and 77 FAD overlay
inputs still matched after the gate; the final executable matches the live
run's checksum above. Canonical differences are the declared dependency
overlay config/lock and the subsequently edited README; the three changed Rust
source/test files match the validated snapshot exactly. Gate-log SHA256:
`ef8a51b751fa2db6d7e96e983af6726563b651e2f2f643f8d8e2b28d13b9a5c6`.
No UBS waiver, parent push, or release was performed.

Doctor multi-row diagnostics (2026-09-09, bead `9lz4y`): both the initial
integrity probe and the post-promotion probe now use the same row collector.
The old single-row API replaced real multiple findings with a row-count error.
The collector inspects all rows, retains up to 20 diagnostics with an omitted
count, and rejects empty or blank output. SQL-produced diagnostic rows test
the response decoder; they do not purport to reproduce the owner's corrupt
archive. An actual healthy PRAGMA and the existing damaged-archive CLI test
cover real database behavior.

The frozen remote gate passed formatting, all-target Clippy, 13 library tests,
one damaged-archive CLI test, and 68 goldens (82 total). UBS timed out after
300 seconds with zero completed files; its one critical marker denotes that
timeout. The RCH transport ended with exit 143 before the outer receipt; the
terminal gate log was recovered directly, and all 1040 CASS plus 77 frozen FAD
inputs were checked before the worker was reused. Log SHA256:
`b732f710d7ac79a1eaebcbf220275c81b03645594d1a8efa961dc34779c1fe7f`.
Executable SHA256:
`eee469a5de9063f7094213f0406e372869623410a8c0ae0fe2a80394530307de`.
This validates diagnostic handling, not original-archive repair or release
clearance. The strict gate and original bead remain open.

GH422 scoped retry continuation (2026-09-09): the old helper copied only query,
format, timeout, data-dir, session-file and explicit mode. Both timeout call
sites now share a retry assembled from the parsed request, including explicit
non-default `--db` before the subcommand, strict read-only policy, scope,
cursor-resolved pagination, semantic options and output budgets. Relative time
bounds are emitted as absolute RFC3339 instants; options and query use existing
shell quoting and the query follows `--`. Non-UTF8 dataset paths and stdin
scope omit retry advice rather than advertising a different request.

Validation completed on vmi1264463 at 18:52 UTC:
`/data/projects/cass-gh422-retry-fixture-20260909/results/` contains the passing
real-process retry journey (1 passed, 0 failed/ignored, 13.61 seconds), actual
formatter and all-target Clippy results. It exercises setup and metadata
timeouts against two matching sessions, a non-default DB, quoted paths, exact
selected-hit and nonexistent-agent controls, and byte-stable archive checks.
The prior batched gate in `cass-gh422-retry-corrected-20260909/results/`
passed 27 library controls, all 13 search-format contracts and 68 goldens:
109 passing tests across the two runs. This is a small isolated archive,
not reporter-scale performance or independent review.

Both failed attempts remain in the record: `cass-gh422-retry-lld839u6`
stopped before compilation/tests because the parent supplied space-separated
UBS paths instead of the required comma-separated list. The corrected full
gate then exposed a test fixture error: filenames lacked Codex's required
`rollout-` prefix, so discovery produced zero hits before retry assertions.
Only those names were corrected; the original exact assertions still pass.
The full gate's UBS module timed out at 300 seconds with zero completed files;
its synthetic critical count of one is the timeout marker, not a completed
source finding. No waiver, suppression or release clearance follows.

Final production `src/lib.rs` SHA256 is
`ea2941bb1958e5091f24b2fc05823cb8d08e16d69a11c9a4ea0a80bdf52da57b`;
test SHA256 is `5dda6450c3d0268b2d9b58ad69b7c8083aaf0e7a5e6e6b16c791a39ff938ed52`.
The same production ELF was used in both runtime runs:
`62e27a5b3ff52940f62a59d1ec595e974328049fd4970078ea5d5312388c2367`.
All 1040 CASS inputs and 77 frozen unpublished FAD inputs were checked before
and after; only the declared overlay config/lock differ from canonical CASS.
Peer commits `e77f5910`/`fb84420d` captured WIP before validation. Original
`u3vho`, reporter-scale acceptance and release gates remain open; the exit-zero
partial-response contract is unchanged.

GH426 continuation: single-conversation NoMem deferral/quarantine previously
returned `scan_had_errors=false` despite saving no canonical rows. Both paths
now retain the deferred source and mark ingestion incomplete. Streaming maps
those source paths to actual connector names across combined batch ranges;
batch indexing also requires persistence completion before advancing its
connector watermark. Existing global scan and mirror-fingerprint gates consume
the incomplete outcome. Provider aliases are not used as connector identity.
Original bead `fyepq` still carries the full per-source observation ledger and
graceful-stop acceptance; this correction does not complete those requirements.

The remote gate in `/data/projects/cass-gh426-qtehoevg/results/` passed 118
selected tests: 11 library tests, 39 storage parity tests and 68 goldens, with
zero failed or ignored tests. Formatting and all-target Clippy passed. The
extended real storage tests cover retained watermarks during induced NoMem,
unrelated completed-connector progress, successful retry, duplicate-free replay,
and quarantine. Fault injection uses existing test hooks; this is controlled
failure-path evidence, not a reporter-sized interrupted scan. The declared
source digest is e9523fdaa95fde0c3ef27f79923fbade94ae15ee87c96fd6b5bf5c77cf11e6d1;
the reviewed formatted indexer SHA256 is
ce996e8a3dbcb86c946362605fb2feacfa58de0fb9008e0493e881d669e265ff.
RCH job 30013452823036020 on vmi1264463 completed at 17:29 UTC. Final identity
verification passed for all 1,040 CASS inputs and 77 declared connector-overlay
files. UBS completed, rather than timing out: one file, 249 critical labels,
8,581 warnings and 2,653 informational findings, exit 1. These classifications
remain unresolved; no blanket false-positive disposition or suppression was
applied. Gate log SHA256:
1898aa8f2216cfd0255e9d244d3f6ba4f2fc9d377d41898410e9eaebfb8267e6.
Tested executable SHA256:
4c169b7d9f821e9c7808f5dd2ddf1d4a34f1dd53c243321b6fbc334001ecf654.
No release clearance follows from the targeted passes. The original bead
retains both the positive runtime result and the red gate.

September 9 field-evidence update: the GH458 reporter confirmed quality
backfill reuse and publication on the original roughly 547,000-message,
4,054-conversation archive using commit
000301c949df5180c0023ef7f6b19767a0cf9fbc (verified as a local ancestor).
Offsets continued 64 to 96 to 128 across ingest; the completed quality tier
contained 425,762 documents and reported current archive identity while
serving semantic queries. This is reporter-provided evidence, not our own
archive-scale benchmark. The reported approximately five-minute canonical
walk per maintenance batch and lock-held serving interruption remain costs;
the legacy fast-tier vector-space refusal remains a separate unresolved
admission/rebuild problem. See the
[reporter's full measurements](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/458#issuecomment-5604445676).

The new GH390 report concerns a different 3.19 GB archive where both CASS
and stock SQLite report orphan pages. It does not establish another checker
false positive or resolve the older archive's disagreement. No compaction,
integrity-grade downgrade, or normalization override was performed; original
bead `rvbsf` retains the distinction and
[the new report](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/390#issuecomment-5605127509)
must be treated as reporter evidence until an unchanged bundle is reproduced.

September 9 follow-up, original bead `coding_agent_session_search-igh4d`:
the primary writer's catalog probe required `rootpage > 0`, excluding the
actual FTS virtual table, whose root page is zero. The correction uses
`type = 'table'` and leaves all shadow admission/suspension limits unchanged.
The real FrankenSQLite regression starts with an unknown presence cache,
inserts and appends through the primary writer, and checks exact MATCH rowids
and replay conservation without an intervening rebuild. Peer commit
ca621f9d2a3d48ffcc18447132ee4e15787ef61a captured the same reviewed source.

RCH job 30013452823035932 on vmi1264463 completed at 13:37 UTC. Formatting,
all-target Clippy, both the new regression and existing shadow-bound control,
and all 68 goldens passed (70 passing tests). The requested integration target
`storage_parity` does not exist: that stage exited 101 and ran zero tests.
The unchanged-source follow-up, RCH job 30013452823035944, ran the correct
`storage_frankensqlite_parity` target: all 39 tests passed in 19.98 seconds at
13:38 UTC. Both source and all 77 overlay inputs matched before and after;
the production executable remained unchanged. Combined selected coverage is
109 passing tests, with the original target-selection failure retained.
Parity log `/data/projects/cass-igh4d-parity-eu_edv4n/results/parity.log` has
SHA256 460deaa6b6be2cd05b6178f93986da13ba22abe24f620e73ea4bb4efacd6de1b.
UBS completed with 28 critical
labels, 6,851 warnings and 1,132 informational findings, exit 1. These are
scanner classifications, not independently confirmed defects or clearance.

Retained results: `/data/projects/cass-igh4d-9c4zdq75/results/`;
gate log SHA256 8de31c4170e6455797475139d253c9fd43f5f6b200e61a145d5b35a421a410e7.
All 1,040 CASS inputs were stable; the declared 77-file unpublished connector
overlay was verified before execution. Formatted storage SHA256:
dced71065c966c17828cb60a1e3a2e21c75b5f84df39b2566d98775d23bad88d.
Actual executable SHA256:
6c1519374d36301aeb0aec8ccb0818d22b68ec7fa7acd0c6dae3210315e86a90.
This proves controlled storage behavior, not large-archive performance or
release readiness; the required scanner gate remains red.

Scope window: `v0.7.1..6b2ab22d30892fe6f7762d851477feea0e6f80f8`, plus
the local 0.8.0 preparation through da1930c0 and reviewed working-tree repairs on
September 9. This is a release-window update, not a reconstruction of older
entries. Research date: 2026-09-09.

Sources: local Git history and diffs, GitHub release/tag metadata, checked-in
Beads records, current implementation, and the prior changelog, in that order.
AGENTS.md and README.md were read during the active release task. The requested
changelog-md-workmanship skill and its research/linking guidance were read.

## Version spine

- v0.7.1: tag created 2026-08-31 10:52:18 -04:00; GitHub Release published
  2026-08-31 18:47:17 UTC. This is the actual binary-release baseline.
- 0.7.0: crates.io publication described in the existing history; no local tag
  and no GitHub Release in the recovered release spine. Do not invent a tag URL.
- 0.8.0: version metadata prepared locally; no tag or GitHub Release exists.
  Keep its changelog section explicitly unreleased until publication.

## Coverage ledger

The original range contains 253 ordinary commits and five merge commits; remote
main adds one ordinary commit, for 259 total. Coordination-only commits inform workstream
status; they are not counted as product features.

| Chunk | Boundaries | Status | Themes |
|---|---|---|---|
| A | first 90 non-merge commits, 2026-08-31 through early 2026-09-02 | validated | connectors, targeted reconcile, observation safety, test isolation, bookmarks, Pages keys, maintenance |
| B | next 90 non-merge commits | validated | archive stalls, FTS budgets, search freshness, dependency pins, answer-pack verification |
| C | remaining 73 non-merge commits through dbe940c7 | validated | pack output contracts, recovery quarantine, final dependency family, connector routing, backfill paths |
| D | remote main 6b2ab22d, 2026-09-08 | validated | hollow lexical-generation detection, merge-memory bounds |
| Merge reconciliation | five merges in the original range | validated | combined-diff review; dependency, pack, recovery and refresh changes retained |
| E | local issue-fix commits through c865ebc4 and reviewed working-tree fixes, 2026-09-08 | distilled; runtime validation pending | Devin parser/WAL watch, Prime presets/probe, active-source watch retries, legacy FTS preflight, exact resume metadata, resumable semantic reconciliation, doctor truth, schema goldens |
| F | reviewed changes through e3c76fa7, 2026-09-09 | full-suite failures diagnosed; corrected schema verified; remaining fixes under validation | Cursor canonical/search repair, temp-path trace privacy, connector fixtures, backfill process helper, exact connector enumeration |
| G | analytics through e93659e3 plus reviewed formatter follow-up and unpublished upstream connector source, 2026-09-09 | 127 targeted CASS tests passed; Clippy and subsequent formatting passed; completed UBS scan remains red | stored workspace analytics conservation, replay and rollback; Copilot workspacePath alias |
| H | reviewed GH422 process regression on c74277eb and the same connector overlay, 2026-09-09 | corrected regression passed in 22.93 seconds; formatting and all-target Clippy passed; prior UBS failure retained | search-triggered stall containment, lock release, durable checkpoint and cold lexical recovery |

## Publication follow-through

- Already-published 6b2ab22d was integrated without conflicts or overwriting
  local version/changelog edits. Its new code/goldens still need release validation.
- At actual release publication, replace the candidate's pending date/state and
  compare-to-main link with the published version metadata. Do not do that early.

## Chunk A findings

Reviewed all 90 subjects and representative diffs/statistics. The prior draft
missed Muse (9a1da8e9), targeted reconcile (20970d4d), remote stdin (fb8d93a4),
Pages recovery locking (b0e1f216), ANN identity (59aab892), and zero-norm vector
recovery (3375db23). Added those to the capability guide and change entries.
Bookmarked CLI (b4b79289), maintenance heartbeat/resume (81ea0649), and the
multi-surface bridge landing (5f059384) anchor existing claims. Early test-only
environment isolation and fixture repairs are enabling work, not user features.
The release timeline now distinguishes the published v0.7.1 release from the
unreleased candidate, and avoids URLs for nonexistent v0.7.0/v0.8.0 tags.

## Chunk B findings

Reviewed the next 90 subjects and relevant implementation diffs. Important
landings: refresh failure backoff (2651e363), inline/page FTS time budgets
(f5625f73), shared message-count bound (f5a7c0ec), checkpoint deadline (b06ba9d0),
fingerprint-covered age (6a398535), skip-open fingerprint honesty (b1a0bc79),
and explicit deferred integrity status (63d47293). The fsqlite 0.3.15 experiment
was reverted; it must not appear as the delivered dependency version.

The draft overstated the f35f25d0 savepoint change as resolving large-archive
stalls. It now describes the actual batching change and its limits. An older
salvage paragraph contradicted later identity quarantine and called type
mismatches proof of page aliasing; corrected it to the final behavior. The
second Fixed heading is now validation/documentation, keeping the timeline
and capability guide readable without treating fixture repairs as features.

## Chunk C findings

Reviewed all remaining subjects, the net production-file diff inventory, and
representative implementation changes. The final engine is 0.3.18 (3babc08b),
search is 0.4.3 (6fb90206), runtime 0.4.10 and dispatch wiring 257204c8. Preserve
the distinction between CASS's Devin adapter and FAD's disabled devin feature.
Backfill path resolution (bcaaf31a), full-scan cutoff handling (aba176ba), and
deferred revision-pinned Git history (2aa093d5) were checked directly.

The pack wave spans verified/redacted citations, full-digest base32 IDs, literal
Markdown, shorten-before-drop selection, serialized-output admission, and
explicit field/skill controls. The draft now has navigation links into that
wave instead of relying on the feature list alone. Beads were read by exact ID;
only qhiv2/ctigq/nsleh are presented as closed examples. 2l1b0.20 and 91njy stay
open/blocked, and the text does not convert targeted tests into full conformance.

## Chunk D findings

Fetched origin/main without changing the working tree. Reviewed the full commit
message/change inventory, hollow-verdict arithmetic and checkpoint eligibility,
the live-count reporting projections, and byte/hull merge planning. Missing
counts do not establish hollowness; fewer than 50% of certified documents does.
The actual recommendation is plain `cass index` (with `--full` doing a full
rescan); the commit message's older --full-only wording is not authoritative.
The merge estimate charges 128 bytes per covered docid plus input file bytes;
oversized singleton inputs are left unmerged. Describe this as a bound on
planned merge output, never a total-process RSS ceiling. GH456/GH457 are closed
on GitHub, but this candidate has not yet executed their new regression tests.

## Validation to date

- Structural validator: exit 0; its sole warning concerns bare commit hashes
  in older entries outside this update. No structural errors.
- Pinned Beads line links checked against dbe940c7: qhiv2/ctigq/nsleh closed;
  answer-pack conformance in_progress and release acceptance blocked.
- GitHub resolves both dbe940c7 and 6b2ab22d; no 0.8.0 release/tag exists.
- Live-link validator: exit 0, all first 40 HTTP links resolved without a link
  warning. That covers all 37 original new-section links. The subsequently
  linked upstream WAL fix resolved via GitHub API to
  8d012706a150be55f0b342937e1d8a1e86c940fb. Remaining validator notices concern
  bare hashes in older entries and the explicit 40-of-536 link scope.
- git diff --check passed. No Rust test was required for these documentation
  edits, and the active source validation was left unchanged.
- Five merge commits were inspected separately so --no-merges inventory did
  not hide reconciliation changes. No additional independent feature wave was
  found beyond the dependency, refresh, recovery, and pack changes described.

## Chunk E findings and current limits

Read the post-research commit inventory and complete production/test diffs.
Local Devin activation is in 6ec77cf7, with actual schema/WAL/nullable-store
tests in 0217c2f2 and 776d4a0a. Prime presets/probe and its real CLI journey are
in 6dba9756. Watch retries began in 58c7bc96; the working tree additionally
propagates callback failures so pending sources survive. Doctor queryability
wording is in e0e5603f. GH440 exact counts/fingerprints are in 32d27ab7, with
the test ordering corrected to inspect metadata before search can heal it.
GH413's newly reported legacy restart bypass has a reviewed working-tree
preflight fix and exact-route regression. Local commit URLs are not added
before the commits are published; the changelog uses live issue links meanwhile.

Old-binary remote controls independently reproduce both the GH440 metadata
defect and GH413's ordinary-versus-legacy shadow-drop discrepancy. These are
small generated archives, not the reporters' large archives. Candidate F6
at source SHA33904882b6d85056343395ffb8ab0e9b9db243abd088d6fe2360c8e35703205e
has passed source transfer and formatting. Clippy failed because a watch-test
helper used an undeclared dependency; the helper now uses standard FileTimes
in the next candidate. All runtime stages then failed before test execution:
the cached runtime build script rejected Devin using the old feature list,
although the frozen build.rs contains Devin. Source timestamps and both build
script executables confirmed stale Cargo reuse. The gate now refreshes verified
input timestamps without changing bytes; its 45 shell checks were independently
re-executed remotely. No runtime result or chacha20 upgrade pass is claimed for F6.

The existing "Devin feature disabled" statements are obsolete and were
corrected. Full integration remains narrower than registry activation:
Devin's database-file WAL routing is implemented with two actual held-writer
commits awaiting F7 execution. Prime's upstream configured-directory/direct-file
admission repair passed 18 real parser/root tests, check, Clippy and formatting
remotely on the connectors feature set; it is not published or adopted by CASS.
These stay in the original issues; no acceptance was moved out to manufacture
closure. Devin's pre-existing swallowed transient scan errors remain unresolved.
FAD's source-boundary seam for GH426 is now published, while CASS's exact
per-source transactional ledger remains unimplemented. Historical release
entries retain the state applicable to their original release.

F5 verified60 hollow-generation regressions and68 golden checks (plus68
regeneration executions); its runtime schema test failed on the missing
selftest command mapping, now repaired for F6. The11 reviewed golden diffs
contain only206 insertions for live_documents/hollow. Strict UBS remains red
or incomplete in retained gates; scanner defects found by source review are
not clearance. No0.8.0 release, tag, or cross-platform artifact exists yet.

GH458 was independently traced to exact-fingerprint checkpoint rejection;
retaining a cursor alone would skip appends to earlier conversations. The
reviewed implementation reuses exact content/provenance identities and applies
the existing batch caps only to missing embeddings. Independent source review
found and the implementation corrected three additional holes: stale readiness
through complete shard metadata, missing staging files with surviving cursors,
and an unrelated destination WAL at final publication. Seven real semantic
regressions and two competing-process CLI lock tests await remote execution.
The algorithm scans canonical identities on each batch; it is not a constant-time
append path or an archive-scale performance result. F7 source
1b6796825c83f79c9d471f03c19ac49dceaddde4c7e02f55b5f285148e9f69ee passed
source identity and formatting, but Clippy found an unavailable test helper API
(`assert_cmd::Command::as_std_mut`). The correction uses an owned standard
process command and preserves the existing assertion wrapper and deadlines;
independent review checked the pinned dependency API. The library stage passed
279 tests, including all seven new backfill cases, with one large-archive test
ignored. The connector stage passed 14 tests and failed the new Devin watcher
test because it waited for an INFO message suppressed by JSON mode. That test
and the equivalent watch helper now explicitly request verbose logs; their
deadlines and assertions are unchanged, and corrected execution is pending.
The actual native MiniLM bundle was downloaded to isolated test storage and all
five manifest sizes/checksums matched. An old-binary control first encountered
a fleet refusal, then failed CLI argument parsing before model installation or
inference. The corrected probe also blocks ancestor dotenv files and requires
exact agreement on all six fixture documents. The corrected F7 native admission
was refused with RCH103/queue_timeout before inference; model execution is still
pending in the combined F8 admission.

F7 terminated with 697 passed, six failed and three ignored tests across 13
executed test binaries. Separate Clippy and backfill integration compilation
failed on the unavailable helper API; the latter ran zero tests. The ordinary
and legacy FTS restart regressions and the interrupted-commit metadata test
passed. All five watch failures stopped at the suppressed INFO startup signal.
The populated introspection comparison exposed eight undeclared runtime paths:
the orphan-blob count/bytes, rebuild engine_incompatible on four surfaces, and
two triage inspected fields. The prepared correction follows the emitters and
adds actual zero-budget triage coverage without widening ordinary status types.
The corrected watch/lock/schema tests await F8; its pinned formatter output was
reviewed and applied manually. F8's 1040 input files match canonical source at
d176dbbb610b23485d929b755306dac72ed312253cb5bf499172b9bbb7100870.

The two isolated patch-level lock updates are now canonical: chacha20 0.10.2
and libssh2-sys 0.3.3. Only their version/checksum records changed; F7 executed
the nine real crypto tests and the explicitly selected Docker SFTP fallback
test successfully on that exact lock. Those tests do not clear the unrelated
F7 failures. Strict UBS actually timed out at 300 seconds, so it remains an
incomplete blocking result. No release publication is implied.

F8 passed all-target Clippy with `-D warnings` and formatting. The connector
target passed all 15 tests, including the two held-writer Devin WAL updates;
the watch target passed all 69 tests, including the four deferred-source
journeys. The backfill target ran five passing tests and two failing lock tests
(one live-archive test remained ignored). Both losing processes returned exit 7,
but the new helper incorrectly read stdout and expected a flat envelope. The
actual CLI emits `error.{code,kind,retryable}` on stderr. The helper now asserts
empty stdout and that exact stderr contract; all downstream conservation and
lock-lifetime assertions remain.

The schema test reached its added budget-fallback invocation, which failed
argument parsing because `--timeout 0` is prohibited by Clap. Internal support
for a zero budget was not proof that the public command accepts it. The test
now passes the valid 1 ms budget, below the production 25 ms response reserve,
which deterministically leaves readiness probes uninspected. Production CLI
bounds and schemas were not relaxed. These two corrected test files await a
follow-up run; the frozen F8 failures and its native-prerequisite refusal remain
part of the record.

The independent native probe passed on omarchy (RCH 30012625538515271,
2026-09-09 00:01 UTC). All 26 CLI commands exited zero. Seven real MiniLM
quality batches added [1,1,1,1,1,1,0] documents; final lexical and semantic
results agreed on all six source identities. Stale and partial quality assets
remained unready. Source, executable and official model identities were
unchanged before and after execution. The executable SHA256 was
669424266abd67116e4ecb00684a14a689afc903b48b476363679e0f79a827e6.
This proves the small native lifecycle, not archive-scale throughput or the
separate process-lock tests. Results are retained under
`/data/projects/cass-gh458-f8-native-independent-la2o7gjv/results/`.

F8 also passed 96 CLI indexing tests (two ignored) and all 68 contract goldens
in both regeneration and verification. The three changed golden files were
reviewed independently and applied only after their canonical base hashes
matched: introspection schema, its shape, and generated schema documentation.
They contain the diagnostic additions and triage-only unknown-value schema
changes described above. Both corrected test files passed remote formatting.
Strict UBS again timed out after 300 seconds. The combined gate remains red.
Its recorded source-stability failure included changed golden files; growing
untracked gate results inside the checkout also affected the digest, so golden
regeneration alone does not explain that result. Reviewing the changes does
not retroactively make the original gate green.

## Full-suite result and September 9 follow-through

F8 completed at 01:43:40 UTC with exit 1. The default non-browser Rust run
executed 259 library/binary/integration targets: 15,125 passed, 39 failed and
92 ignored. Two documentation batches added eight passes and 20 ignored tests,
for 15,133 passed, 39 failed and 112 ignored overall. The count uses each parent
target's terminal result; a nested child result is not counted twice. The full
log SHA256 is 03df48e08ca6ae9dff66e7bc99186a5b54f84089eb3cce0ebf73c9bc8f38d453,
retained in the F8 snapshot's `.gate-results/f8-combined/full-rust-suite.log`.
This is not all-features, browser, or ignored live-archive validation.

Eight targets failed. A real trace leaked temporary archive paths; the swarm
redaction policy now handles standard and configured temporary roots before
home-prefix shortening, including Windows and macOS paths. The existing
failing CLI privacy assertion is unchanged. Aider, Copilot and Factory fixture
corrections select their actual admitted roots/connectors; their payload and
ordering assertions remain. Two stale test contracts now recognize the actual
serialized gate formatter and the exact 29 enabled connectors, retaining their
negative cases and strict equality. These changes are in 8f1560f8, 8d45cbc2,
a0ee74b9 and 4e0f846f; their corrected runtime results are still pending.

The two-test follow-up retained the original F8 source/results before editing.
Formatting, targeted Clippy, the schema test (one pass) and all 68 goldens
passed, with source and F8 executable unchanged. Backfill remained five passes,
two failures and one ignored test: placing global `--db` after the subcommand
caused an auto-correction note before the otherwise valid stderr JSON. Commit
e3c76fa7 moves that argument before `models`; it does not strip stderr, relax
the envelope, or change production CLI behavior. The entire backfill target
is included in the next combined gate.

GH459 exposed lossy Cursor workspace inference from hyphenated directory names.
The upstream repair uses explicit `.workspace-trusted` metadata and preserves
unresolved attribution when it is absent or malformed. CASS commit 7537be83
repairs canonical workspace metadata on unchanged-source full scans, records
lexical rebuild debt before the transaction, and invalidates both semantic
workspace identities atomically. Canonical NULL workspace also overrides a
stale legacy FTS value. Tests cover row conservation, replay, interruption,
search filtering and semantic re-enrichment. The parser is unpublished;
registry FAD 0.2.3 has not been replaced. Analytics workspace reassociation is
a separate remaining part of the same issue, not a completed capability.

F9 tests the exact CASS inputs with a declared frozen upstream source overlay.
Two preparation attempts stopped before compilation: one missing tracked file
in RCH transfer, then a targeted Cargo update that changed unrelated dependency
edges. Both are retained failures. The corrected admission verifies all 1,040
CASS inputs and 76 upstream files, changes only FAD's registry source/checksum
in the lock, and retains the existing locked compiler/test/golden/UBS checks.
No overlay result establishes registry adoption or release completion.

F9 completed at 03:18:08 UTC on September 9, with 308 passed, two failed and
one ignored test across 11 binaries. Formatting and all-target Clippy passed;
source identities remained stable. The real Cursor CLI journey passed,
including unchanged-source workspace correction, lexical filtering, semantic
identity invalidation/rebuilding, and replay without duplicate messages.
The trace target passed all 64 tests, Aider 98, Factory 13, backfill seven
(one ignored), scanner-contract tests 18, connector enumeration two, and
contract goldens 68. UBS timed out after 300 seconds with zero completed files;
this remains a blocking failure, not a completed scan with no findings.
The complete gate log SHA256 is
713d168fba8ab5f58e25f89bd11d47f6431d616cc780a9b340c71d5bb6756d51.
Its invocation-built executable SHA256 is
9190abcce479671deb01ad6113fdc526d09ebc6dc4dd2a286cf1c1e20c8e76c2;
the original executable, results and affected inputs were retained before
the next changes.

The library failure was a new fixture precondition: the pinned contentless
FTS implementation returns empty text where the assertion expected SQL NULL.
The correction retains the content-bearing stale-workspace precondition and
every actual search/filter assertion. Copilot passed 11 tests and failed one
real legacy-history workspace assertion. Its parser omits the top-level
`workspacePath` alias; a one-line upstream fallback and three real scan tests
were prepared without changing the original CASS assertion. Both corrections
passed in the subsequent 127-test run below. The earlier upstream Cursor gate
separately completed 79 parser tests and one registry test successfully; those results do not
include the later Windows fixture escaping or Copilot change.

CASS analytics reassociation is implemented and reviewed, with runtime
validation recorded below. It moves stored workspace contributions in the same
transaction as canonical attribution, preserves measured token/cost values,
repairs canonical no-op replays, and rolls back on missing or underfilled
rollup buckets. Three storage regressions and the expanded real CLI journey
cover shared buckets, trusted-to-unknown transitions, buffered appends, replay,
and rollback. The new upstream overlay remains unpublished and the canonical
registry dependency is unchanged. No release is ready for publication.

The analytics follow-up subsequently passed all 127 selected tests: seven
library regressions, the expanded real CLI workspace/analytics journey,
39 storage parity tests, all 12 Copilot tests, and 68 contract goldens. All-target
Clippy passed. The three analytics regressions positively exercise existing
stored token/cost amounts, shared workspace buckets, canonical no-op repair,
buffered appends/replay, and full rollback after a late model-rollup failure.
The CLI test begins with populated metrics and ledger rows and checks amount
conservation after trusted and unknown attribution, replay, and semantic repair.
These are controlled fixture results, not execution on the reporter's archive.

The new upstream snapshot also passed its focused parser checks. The first
run passed 21 Copilot, 13 Cursor agent-transcript, and one registry test but
failed Clippy on two timestamp literal spellings. After adding digit separators
without changing their values, the corrected run passed formatting, locked
all-target Clippy, and the three new Copilot tests with all 77 inputs unchanged.
Its source equals the CASS follow-up's declared overlay. The upstream commit
is held under the parent repository's mandatory UBS rule: its precommit scan
reported findings on unchanged Cursor shell code and broad test warnings.
No scanner waiver, dependency publication, or issue closure was made.

The CASS follow-up's first formatting check reported one array/iterator layout
in a new storage test. The earlier pinned formatter emitted source through stdin
but did not run a subsequent Cargo formatting check; its successful process
exit was insufficient. After preserving the completed runtime receipt, remote
formatting-only job 30012625538515432 passed at 04:20 UTC. The complete returned
patch changes only that test's layout and trailing comma; it was reviewed and
applied to the identical canonical input. All 1,039 other source inputs and the
tested executable remained unchanged. Formatted storage SHA256 is
3ac735a21b84fb85a88dd9ef8ea93db62e22055d308fcc731b361cea2556a459;
the runtime used the pre-format storage source
7b1084f03bcd9771560c5ca0174c9956b90db08f71fe81f60e0f24791cddb8d2.

The analytics gate, job 30012625538515403, completed at 04:12 UTC. Unlike F9's
timeout, its strict UBS scan finished: three files, 108 critical findings,
9,591 warnings and 1,818 informational findings, exit 1. Independent review
of every displayed critical location found test assertions, fixed executable
paths, query syntax, cache hashes, and internal table/metadata names. The report
only displays capped samples; unshown findings remain unclassified. No blanket
suppression or scanner clearance follows from that review. Source inspection
also found an unquoted argument-array expansion in the scanner's Cargo wrapper
that explains its misleading build-clean labels; the separately executed
Clippy and formatting receipts remain the actual compiler evidence.

The retained gate and formatter results live under
`/data/projects/cass-gh459-analytics-followup-e4o6via2/results/` and
`/data/projects/cass-gh459-final-format-ktnc0se0/results/`. The tested executable
SHA256 is 1fe22d59a1c4e21a315253c1ec6e377f59a6a68449422672701c4ba6936e10ea.
Peer commits e93659e3 and c74277eb captured the existing source and documentation;
their presence is not a green gate. UBS remains blocking, the upstream connector
candidate remains unpublished, and the issues and release remain open.

## GH422 search-triggered watchdog verification

The new Unix regression in `tests/e2e_lexical_fail_open.rs` exercises the
ordinary search process's supervised, in-process lexical refresh. It reuses
the existing post-commit pause hook; no production code or new hook was added.
After a real scratch-index commit, it checks the same search PID holds the
index lock, durable checkpoint progress remains incomplete, and metadata
heartbeats advance while forward progress stays frozen. The process must
itself exit 70 with the structured `index-stalled` error before the artificial
pause finishes. A harness timeout or cleanup kill cannot satisfy that assertion.
The next cold query must publish complete lexical assets, return both expected
message identities exactly once, and preserve canonical IDs and message data.

The first remote gate, job 30012625538515443, completed at 04:52 UTC with
84 tests passing and this new test failing before its watchdog assertions.
The parent-added sentinel precondition incorrectly expected a checkpoint file
path where the hook records the index directory. The child guard reaped the
process; that run proves neither watchdog exit nor recovery. Formatting,
all-target Clippy, 13 library controls, three other CLI controls, and 68 goldens
passed. UBS completed with 98 critical findings, 859 warnings and 121
informational findings, exit 1. Its failure remains blocking.

Only the mistaken path assertion was corrected. Follow-up job
30012625538515448 completed at 04:59 UTC with formatting, all-target Clippy,
and the exact new regression passing: one passed, zero failed, zero ignored,
22.93 seconds. All checkpoint, heartbeat, exit, lock, deadline, asset and
conservation assertions remained intact. The formatter's complete returned
diff was reviewed and applied manually. Final test SHA256 is
3497dea3230c3be4a5313a61eaf45d471fc518f20bd18047b5caf686e3ea9774.
All 1,040 CASS inputs and 77 declared connector-overlay inputs were verified
before and after execution. Against the canonical checkout, only the declared
connector-overlay Cargo configuration and lock differ; production executable
SHA256 remains 1fe22d59a1c4e21a315253c1ec6e377f59a6a68449422672701c4ba6936e10ea.

Both runs are retained under
`/data/projects/cass-gh422-inline-watchdog-39_y0xyi/results/` and
`/data/projects/cass-gh422-sentinel-fix-bnxlkyrs/results/`.
The corrected `gh422-runtime.log` SHA256 is
960da60a1e6803b5e3d2d5818b1ebc97e8a089c3bcb78ec64073bce2d345d2ba.
The original bead `coding_agent_session_search-u3vho` and
[GH422 progress comment](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/422#issuecomment-5596078395)
record the positive result and its limits. This is controlled-fixture evidence
for existing containment behavior, not reporter-sized archive acceptance or
a change to the separately reported empty-success `--timeout` response.
The issue and release remain open; the focused follow-up does not clear UBS.
