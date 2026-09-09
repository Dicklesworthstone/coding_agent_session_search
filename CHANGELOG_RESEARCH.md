# CASS 0.8.0 changelog research

Scope window: `v0.7.1..6b2ab22d30892fe6f7762d851477feea0e6f80f8`, plus
the local 0.8.0 preparation and issue-fix follow-through through e3c76fa7 on
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
