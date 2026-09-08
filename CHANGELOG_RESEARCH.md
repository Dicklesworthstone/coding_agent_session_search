# CASS 0.8.0 changelog research

Scope window: `v0.7.1..6b2ab22d30892fe6f7762d851477feea0e6f80f8`, plus
the local 0.8.0 version preparation. This is a release-window update, not a
reconstruction of older entries. Research date: 2026-09-08.

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
