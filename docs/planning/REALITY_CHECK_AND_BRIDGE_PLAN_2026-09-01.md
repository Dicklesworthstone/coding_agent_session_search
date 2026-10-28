# Reality Check and Bridge Plan — refreshed 2026-10-28

## Release qualification follow-up: 2026-10-28 (#537 / #530)

**Release verdict: BLOCKED. No corrected release or release candidate was built
or published in this follow-up.** The latest GitHub release remains
[`v0.10.0`](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.10.0),
published `2026-10-02T02:54:21Z`. There is no first released version containing
the subsequent #530 corrections to recommend, and no justified release date.
`release-bar-decision-hutyl` remains an unresolved owner decision; its proposed
"no worse than v0.10.0" bar is not approval to remove prerequisites.

This is an additive qualification and operator-guidance update. It supersedes
the older assessment's uncertainty about the probed Linux executable's release
provenance, and its description of `.84` / `.85` as still unimplemented. It does
not supersede the existing release requirements or certify current source.
No production code, dependency, test assertion, skip, scanner policy, or bead
status is changed by this documentation update.

### Identity and new evidence

The reviewed source and tracker snapshot are `main` at
`19207dcf912fb8d6e104116cde6976eeb701c1d4`. The issue discussions include
[the Oct27 freshness report](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/537#issuecomment-6456648306),
[the existing qualification follow-up](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/537#issuecomment-6460007754),
and [the #530 correction history](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/530#issuecomment-6378296003).
Already landed corrections include surplus-document refusal (`b605426e`), archive
writer admission (`35fcb9d1`), append-safe targeted recovery (`bef5ae45`), and
avoiding whole-history semantic preparation (`fbff882b`). Workspace exclusions
and selective history retention are also implemented and tested in source; their
presence on `main` does not make them available in v0.10.0.

The actual published Linux amd64 archive was downloaded and matched its GitHub
asset digest, `SHA256SUMS`, and release manifest:

- Archive SHA-256: `638ea8a8a9023c2e046bf8868392e6191218b89cf006e81af3ca205b546c4681`.
- Extracted executable SHA-256: `d221153d8e0a79ae37eb492855d32cd2aadb98e707c74c37445f779a46c626dd`.
- Release source commit: `2bfe708d35734656f0d34eab112bf051e354519c`.

The executable is byte-identical to the previously probed binary whose provenance
was unknown. This establishes that executable's identity as the published Linux
asset; its own `build_commit: unknown` is still not candidate source attestation.
Its in-memory selftest succeeded without accessing an archive. Other platforms'
listed digests were inspected, but their binaries were not downloaded or run.

| Execution or check | Observed result | Evidence boundary |
|---|---|---|
| Published v0.10.0, isolated Claude JSONL, 40 conversations / 800 messages | Initial index exit 0, 5.393 s, kernel peak RSS 137,652 KiB; native status shows 800 canonical / live / checkpoint messages and matching completed fingerprints; unique tail search succeeds | Small released-binary baseline, not current-source or large-archive qualification |
| Same baseline through the proposed qualification lane | `NO_VERDICT`: released JSON lacks `indexing_stats.deferred_active_sources` | Missing candidate evidence is not silently treated as zero deferred sources |
| 31k / 1.71m synthetic released-binary baseline | Externally stopped with SIGKILL after 808.181 s when shared-host free space crossed the guard's 3 GiB threshold; kernel peak RSS 616,124 KiB; sampled logical reads 31,502,555,478 bytes | Initial full build incomplete; **NO_VERDICT**, not the reporter's incremental workload or a candidate pass |
| Proposed qualification runner's own controls | 28 tests pass, including actual child timeout, memory-budget rejection, and process reaping | Tests the runner's evidence handling; does not execute the CASS Rust integration tests |
| Proposed freshness change, pinned nightly rustfmt | Parse/format check exit 0; `git diff --check` exit 0 | No compiler, Clippy, or Rust runtime verdict |
| Frozen proposed Rust files, pinned strict UBS with AST engine | Exit 1: 46 critical / 10,442 warning / 2,873 informational findings | Red; all 46 critical sites are unchanged baseline code, but the warning set is not fully dispositioned |
| Frozen qualification runner and controls, pinned strict UBS | Exit 1: 0 critical / 2 warning / 162 informational findings | Red; narrow review found no control transfer in the flagged `finally` and confirmed `wait4` reaping, but no scanner-policy correction was qualified |
| Required `scripts/gate.sh --docs-truth` with indexing, archive, privacy, robot, Pages and search integration targets | `STAGE=rch EXIT=127`; source/fmt/Clippy/test/golden/UBS terminal receipts missing | RCH is unavailable in this execution environment. **NO_VERDICT on current-source validation**, not a failing Rust test result |

The full-size fixture has exactly 31,000 conversations, 1,710,000 messages,
1,397,449,788 source bytes, and one 32,768-message conversation. Its inventory
SHA-256 is `4289a86d2fcb17dece7a711bd6eae7a3428774bf5d912cf098246fc0526d91b0`.
It contains mixed natural-language/code sizes and dates, but only Claude JSONL;
it does not reproduce the reporter's provider mix, shared databases, existing
canonical size, or raw-mirror history. The binary's actual exclusion event confirms
exactly one enabled connector; the retained invocation environment supplies its
explicit synthetic Claude root. The process used a 6 GiB RSS
observation limit and 1,200-second initial-build deadline; resource readings are
for the measured child, not a hard process-tree/cgroup memory proof. Sampled
`/proc` I/O counters are lower bounds, and cache state was not controlled.
The scale run started before the runner's final self-identity check was added;
its retained raw observations cannot qualify the final runner. Concurrent writers,
incremental passes, crash recovery and candidate search continuity did not run
in that baseline lane.

The disk guard verified the executable and process identity (PID 32270) before
signaling only that owned synthetic benchmark process group at
`2026-10-28T20:31:45Z`. Its observation was 3,114,962,944 free bytes against a
3,221,225,472-byte threshold. The raw runner receipt is preserved as
`indexing_verdict: FAIL`, `initial: exit -9`, `release_verdict: NO_VERDICT`; the
known external resource stop means there is no completed product qualification
verdict. It is neither an application crash diagnosis nor a reproduced scheduled
incremental timeout. The whole shared-host free-space decline is not attributed
to CASS. The measured CASS child's sampled I/O also records 310,702,080 physical
read bytes and 8,939,986,944 write bytes; these are incomplete-run observations,
not a completed build cost or an incremental budget.
After the stop, all 31,000 original source files were independently checked against
the retained inventory: sizes and SHA-256 values were unchanged. The fixture's
data directory occupied 11,622,572,032 allocated bytes at that observation. This
establishes source preservation and a file-footprint observation, not canonical
integrity, cleanup completion, or a safe steady-state storage bound.

### Successful indexing with a stale timestamp

`persist_final_index_run_metadata_with_writer` deliberately preserves a successful
indexing result when the final metadata write fails, logging the failure instead
of discarding already durable indexing work. Consequently, exit 0 with an
unchanged `last_indexed_at` is a real possible outcome. This explains a class of
the reported symptom; it is **not a demonstrated root cause on the reporter's
host**. The two metadata writes are not atomic: a scan marker can commit before
the indexed marker fails, and a close failure need not imply rollback.

A focused, uncommitted correction has been prepared to expose an optional
`indexing_stats.freshness_metadata` receipt with `status: persisted | deferred`,
the attempted indexed timestamp, and the confirmed durable scan/indexed values.
On failure, confirmed values are null: they are unconfirmed, not proof of absence
or rollback. A run with no write attempt has no receipt. The receipt is cleared
when a reused progress object starts another run. This preserves the existing
durable-index success policy while making monitoring distinguish a completed
indexing operation from confirmed freshness persistence.

Prepared Rust controls cover actual read-only write refusal, partial metadata
writes and successful retry, denied archive-writer admission, reuse of progress
across runs, two unchanged CLI passes, canonical-only force rebuilds, and held-open
source deferral followed by catch-up. They have **not been compiled or executed**
here. Neither this code nor the new runner is committed by this update: the
required current-source gate is unavailable and their strict scans are red.

Keep the existing meanings distinct: `last_scan_ts` can remain unchanged when
sources are deferred; canonical-only rebuilds must not imply a source scan; empty
watch callbacks must not manufacture freshness. A fresh timestamp also does not
prove every active source was ingested. On the reviewed source, reusing an
`--idempotency-key` can replay a cached successful result instead of retrying a
deferred metadata write; it is not a recovery mechanism. The reporter's examples
do not establish that this option was involved.

### Requirements that still block release

These are acceptance obligations, not a request to remove safeguards:

| Requirement / owner | Outstanding acceptance |
|---|---|
| Exact-source gate: `55wh0`, `5v57k`, `yrjna`, `.25` | Run the complete required RCH gate on one frozen source identity with positive test counts and terminal receipts; resolve real failures and qualify any justified scanner correction with controls. The prior retained full selected run reports 12 HNSW failures and 106 critical / 20,722 UBS warnings; those are prior-snapshot findings, not new executions or directly comparable to this two-file scan. |
| Indexing cost and integrity: `3zq55`, `irqgt`, `p3gh0`, `.46`, `.72`, `iify0`, `.24` | On a current-source release candidate, run the full 31k / 1.71m archive, unchanged passes, append, and concurrent writer cases. Measure end-to-end RSS, I/O, disk growth and elapsed time, including startup/finalization; prove no duplicate/missing messages, no unnecessary rebuild, honest active-source deferral and eventual catch-up. The earlier 31k-conversation incremental fixture contained 310k messages, so it is not a 1.71m-message qualification. |
| Lexical generation and recovery: #530, `h3jnv`, `lndgg`, `.48` | Preserve equality between canonical snapshot fingerprints, checkpoint counts and the published live generation; refuse surplus/stale generations. Kill at the staged commit/checkpoint gap, resume without source loss or duplicate publication, and keep the previously published generation searchable during rebuild. All of this requires actual concurrent execution, not merely test definitions. |
| Native logical-archive recovery: `svhcf`, `p94yt`, `.34` | Run the original populated probes without their current skips on approved, uniformly pinned released dependencies. `publication_materializes_wal_rows_without_borrowing_replay_sidecars` is blocked by the 0.4.12 read-only freelist-integrity false rejection; `explicit_or_unattested_collations_keep_binary_order_and_verify` is blocked by PK/collation introspection. Preserve WAL materialization, integrity checks, no-clobber publication and source immutability. Latest upstream release was freshly checked as v0.4.12; the known upstream main fix is not yet in an adoptable tagged update. |
| Semantic/native ANN: `of9i7`, `ds7uy.3.3`, `fdvur` | Resolve the identity-complete FSVI v2/native-reader mismatch and qualify actual owner-supported incremental graph updates and tombstone admission. Do not downgrade fixtures, drop identity checks, use private upstream layout, or call a full rebuild incremental. |
| Privacy and robot errors: `.84`, `.85`, `.23` | The shared export privacy boundary and shared archive error envelope are implemented. Retained native evidence for compile inputs verified identical at `870797c0` includes five CLI privacy controls, a decrypted encrypted-payload check, source/DB/WAL immutability controls, and the archive error negative. The later rerun is self-verification, not a new full-gate pass. These fixes remain gate-blocked rather than unimplemented. Requalify the exact candidate, raw/canonical HTML and generic exports, TUI policy parity and explicit sensitive-output receipts; `.23` separately retains browser/publication recovery acceptance. |
| Historical archive and platform acceptance: `.2`, `.4`, `.41`, `.45`, `.47`, `scohn`, `aegfi`, `yviq2` | Obtain the required damaged/historical archive, orphan/legacy-FTS writer-reader, and Windows writer evidence; finish DSR cross-platform build/install/artifact acceptance. DSR is not available here. The previously recorded macOS disk-floor failure is historical evidence, not a fresh fleet-health measurement. |

The durable release graph still lists nonclosed blocking dependencies of `yrjna`:
`55wh0`, `5v57k`, `irqgt`, `p3gh0`. The `.25` acceptance bead additionally retains
`.11`, `.2`, `.20`, `.22`, `.23`, `.24`, `.4`, `.41`, `.45`, `.47`, `.84`, `aegfi`,
`iify0`, `scohn`, and `yviq2`. Retrieval, answer-pack, terminal, privacy and sync
acceptance cannot be silently reclassified as optional to ship this fix.
The `yviq2` title is stale about missing configuration: the DSR project exists;
its remaining acceptance is the blocker. GitHub Actions requirements in historical
text are superseded by the permanent Actions ban. The FAD git dependency blocks
crates.io publication of this manifest, not by itself the locked-source GitHub
binary channel. Neither distinction clears the actual release gates.

### Supported guidance while deployments require released binaries

- There is no released upgrade containing these corrections yet. Do not recommend
  a main-branch build, an unqualified candidate, a fabricated version number, or a
  release date to a released-only deployment.
- Serialize scheduled indexing so one invocation owns the work. Provide a quiet
  maintenance window long enough for one complete pass; avoid a loop of forced
  rebuilds killed at every scheduled deadline. This is operational mitigation,
  not a claim that v0.10.0 can meet the reporter's 20-minute window.
- `CASS_AUTO_REFRESH=0` and lexical search with `--no-maintenance` can stop
  read-triggered maintenance while investigating; they do not disable OS-scheduled
  index jobs. For example, `CASS_AUTO_REFRESH=0 cass search '<known recent term>'
  --mode lexical --no-maintenance --robot --robot-meta` observes lexical coverage
  without establishing canonical archive integrity.
- Retain stdout JSON, stderr, exit status, start/end time, `status` / `health`, the
  JSON `last_indexed_at` and its age, and searches for known recent messages.
  Compare the timestamp across invocations. Exit 0 and a convenience freshness
  boolean alone do not establish successful catch-up. The new deferred-source and
  metadata receipts described above are unavailable in v0.10.0.
- v0.10.0 supports `cass forget --source-glob '<reviewed-glob>' --json` to preview
  deletion selection, followed by `--apply` only after scope review and a verified
  backup. Preview does not apply the requested deletions, but the tag opens the
  archive through the ordinary writable storage opener; do not describe it as a
  guaranteed mutation-free inspection. Glob matching crosses directories, and
  shared stores cannot be separated into individual workspaces by a file glob.
  This removes CASS history, not source files. Universal workspace exclusion and
  workspace/age-selective forgetting on main are not released options. Apply can
  rebuild derived indexes, and a later change to a shared source can reintroduce
  its forgotten conversations unless the supported source scope is excluded.
- `CASS_EXCLUDE_PATHS` in v0.10.0 applies to Claude, Codex and Pi, not every
  connector. Validate the installed provider's supported scope before relying on
  it to prevent re-ingestion of forgotten history.
- `cass mirror prune --older-than 30d --json` previews eligible raw-mirror removal;
  use its provider/source-path selectors and inspect the result before `--apply`.
  Do not prune the only surviving evidence needed for recovery. Raw-mirror
  retention is separate from canonical history and lexical freshness. Disabling
  new mirror capture reduces recoverability and is not a general indexing fix.
- The demonstrated default raw-source export privacy defect remains in v0.10.0.
  Keep its HTML/generic exports private until contents, titles, filenames and
  receipts have been inspected and redacted in a separate sharing copy. Index-time
  redaction and encryption do not make a secret-bearing exported payload safe to
  share. Preserve the original source evidence.
- For suspected corruption, preserve a consistent archive backup including
  committed WAL and original sources; use the supported `cass doctor check
  --json`, `cass doctor repair --dry-run --json`, and backup list/verify/restore
  rehearsal workflow before approving repairs. Do not hand-delete DB, lexical,
  mirror, lock, WAL or SHM files. Avoid stock `sqlite3` against the live v0.10.0
  archive: the known old-engine interoperability failure can leave later writes
  refusing admission. Inspect a consistent copy when a stock-engine comparison
  is needed, including for stock `sqlite3 -readonly`; it is not a substitute for
  native recovery acceptance. v0.10.0's ordinary native read-only opener can also
  perform recovery writes, so deeper doctor diagnostics are not promised to be
  mutation-free. A large archive can return integrity unverified/deferred; open
  success and row counts are not an integrity certificate.

Release delivery can resume after the applicable requirements above have positive
exact-candidate evidence and the DSR artifacts pass checksum, identity, installation
and platform smoke verification. Then name the actual first tag containing the
fixes and provide upgrade/rollback instructions tied to those artifacts. This
follow-up records no permission to lower that bar.

## Assessment: 2026-10-27 (QuietPond)

**Verdict: CASS delivers its core search value as a useful alpha. It is not yet a
fully qualified, consistently bounded, completely integrated realization of its
vision.** The main problems are unfinished serving integration, resource costs on
real archives, specific privacy/recovery contracts, and delivering validated fixes
to users. This is not a project consisting mainly of stubs.

This section supersedes earlier assessments below where they disagree. Earlier
evidence and plans remain intact for provenance. No implementation bead was closed
by this assessment, no safety gate was relaxed, and no production code changed.

### Scope, identity and evidence boundaries

- Reviewed `main` at `1bf194de686566b41d19d2d928a2cde66f92bc84`, 774 commits after
  the `v0.10.0` release commit. Cargo still says `0.10.0`. A live remote-ref query
  found `origin/main` at the same SHA; commit count is delivery context, not proof.
- Read all 663 lines of `/data/projects/AGENTS.md`, all 1,382 lines of repository
  `AGENTS.md`, and all 3,885 lines of `README.md` before source investigation.
  Three read-only subsystem investigations read the founding, search/semantic,
  optimization, storage/recovery, Pages, TUI, analytics and testing plans and
  current contracts. The answer-pack and swarm contracts and prior assessments
  were included. Generic CLI advice is reference material, not an automatic
  expansion of CASS's product commitments.
- Source scale at this SHA: 553 Rust files / 727,187 lines under `src/`. The four
  shared files `lib.rs`, `indexer/mod.rs`, `storage/sqlite.rs` and `ui/app.rs`
  total 301,359 lines. This explains review and qualification cost; line count
  does not measure feature completion.
- Fresh executable probes used `/home/ubuntu/.local/bin/cass`, SHA-256
  `d221153d8e0a79ae37eb492855d32cd2aadb98e707c74c37445f779a46c626dd`.
  It reports `0.10.0`, `build_commit: unknown`, 32 connectors, and no
  `source_ingest_reuse` capability. Reviewed HEAD has 34 runtime connectors and
  that capability. **This executable cannot certify current HEAD or be identified
  as the exact published release merely from its version.**
- The default home archive on this host is uninitialized. Its health/status
  diagnosis and missing-index refusals were truthful; no operator archive was
  indexed or repaired. Positive probes used one isolated synthetic remote mirror
  and `sources reingest`, whose closed local-root map avoids default home scans.
- Evidence is retained privately under
  `/data/tmp/cass-reality-check-20261027-quietpond/`: runtime JSON/exit/stderr
  records, fixture source and outputs, public release/issue observations, DSR/RCH
  receipts, source-review memo, and original tracker snapshot. Temporary paths
  are local receipts, not a promise of permanent artifact hosting.

### What actually ran

| Probe | Observed result | What it establishes |
|---|---|---|
| `cass selftest --json` | Exit 0; in-memory FrankenSQLite typed round trip; archive not accessed | This installed executable is functional; no disk-archive qualification |
| Home `health` / `status` | Health exit 1, status exit 0; `not_initialized`; health latency 8 ms | Truthful empty-install diagnosis, not large-archive latency |
| Synthetic `sources reingest --source fixture --full --json` | Exit 0; 1 conversation / 2 messages, 2,932 ms | Real mirror-to-canonical-to-lexical journey without SSH or home discovery |
| Repeat identical reingest | Exit 0; still 1 / 2, 2,929 ms | No duplicate canonical messages in this fixture |
| Lexical search, `--no-maintenance --robot-meta` | Exit 0; 2 hits, exact total 2, no timeout; synthetic credential absent and `[REDACTED]` present | Real retrieval, index redaction and bounded metadata on this tiny corpus |
| Default hybrid, missing MiniLM | Exit 0; 2 lexical hits; requested hybrid, realized lexical, explicit consent/model fallback | Correct fail-open behavior; no model downloaded |
| Explicit semantic, missing MiniLM | Exit 15, `semantic-unavailable` | No same-dimensional hash substitution |
| `pack --max-tokens 4000 --require-evidence` | Exit 0; `cass.pack.v2`, 2 evidence items / 1 session, strict privacy, no synthetic credential | Extractive pack path works. Remote physical spans are null/`verified=false`; no physical-citation proof |
| Canonical `view` using explicit DB, source, conversation and message index | Exit 0; secret absent | Search identity can reach the redacted stored message |
| Persistent `serve --stdio` | Status, two searches and shutdown all succeed; 2 hits each, second search `reader_reused=true` | Real retained lexical-reader reuse; no semantic/ANN or MCP-handshake qualification |
| Direct-source HTML and Markdown export | Exit 0; synthetic recognized password retained in title/body and HTML JSON receipt | Reproduced privacy-contract defect, now `.84` |
| HTML export of the already indexed synthetic Codex source | Query output redacted, same readable source export contains credential | Raw-source preference bypasses canonical redaction even when indexed |
| Missing-file `archive verify` | Exit 14; one JSON error, empty stdout, missing `hint` field | Reproduced delegated error-shape defect, now `.85` |
| Browser worker Node tests | 51 pass, 0 fail/skip | Real WebCrypto/HKDF/DEFLATE/worker/HTTP behavior; controlled Argon2 input, no browser/SQLite-WASM/native Argon2 proof |
| `scripts/validate_docs.sh --links` | Exit 1; 82 findings across repository documents, none naming this refreshed report | Findings require classification: fragment-only Markdown links are falsely tested as filesystem paths; this is not proof of 82 broken links |
| Full `scripts/gate.sh --docs-truth` | RCH selection outcome unconfirmed; no source/fmt/clippy/test/golden/UBS terminal stage | **NO_VERDICT on current-source validation**, not a failing test suite |

The first `view` probes incorrectly put `--data-dir` on that subcommand and exited
2. The successful probe used the supported global `--db` plus `CASS_DATA_DIR` and
canonical coordinates. Failed probe outputs remain in the evidence directory.
The sub-60 ms figure is an engine claim; a two-message fixture cannot validate
one-shot latency, quality, memory or I/O on a multi-million-message archive.

The full gate's retained receipt is `gate.receipt` plus `gate.console.log`:
`STAGE=rch EXIT=1`, all requested stage receipts missing. The original lease is
`rchw-b9ba40ab-e124-4d90-a9e0-dc2aa58b6495`; same-identity cancellation was not
acknowledged (`not_queued`), and recovery reports the original wrapper absent or
unverified before admission. The job was not replayed or run locally. The raw
lease and errors remain evidence of unresolved execution custody, not permission
to restart shared infrastructure or erase locks.

### Vision checklist and work coverage

`WORKING*` means the narrow installed-executable fixture above passed. It does not
mean fresh HEAD or full release qualification. `PARTIAL` includes a real
implementation whose stated integration or acceptance is unfinished. `UNPROVEN`
means the promised scope lacks current execution evidence. Existing bead suffixes
below use the common `coding_agent_session_search-` prefix.

| # | Testable promise / source | Current reality and evidence | Remaining ownership |
|---|---|---|---|
| 1 | Discover/normalize all advertised providers; README connectors | PARTIAL qualification; FAD registry plus CASS adapters are real (`connectors/mod.rs:380`); 34 on HEAD, 32 in probe binary | `cooow`, `.24`, provider-specific beads |
| 2 | Same completeness and exclusions in batch/watch/retry | PARTIAL: Continue watch/retry constructs upstream connector instead of CASS adapter (`indexer/mod.rs:31522`, `:32152`) | `cooow`, refined this pass |
| 3 | Canonical pure-Rust SQLite production storage | WORKING* for fixture; FrankenStorage persists rows; `rusqlite` dev-only. External `sqlite3` repair subprocesses remain | `.65`, integrity/release owners |
| 4 | Dedup and stable source/conversation/message identity | WORKING* repeated reingest; native-ID/shared-store reconciliation is implemented | `fx9fd`, `.24`, larger corpus acceptance |
| 5 | Cheap unchanged/incremental indexing | PARTIAL: durable reuse exists; Oct27 `3zq55` reports 5.3 GiB logical reads on no-change and 8.7 GiB for one append | `3zq55`, `ayhvu`, `irqgt` |
| 6 | Responsive bounded indexing and durable resume | PARTIAL: resident controller and staged checkpoints are wired; process-wide peaks and startup/finalization still matter | `.46`, `.72`, `xcqqa`, `xn5a4`, `index-bounded-shutdown-gm63q` |
| 7 | Preserve raw evidence without uncontrolled growth | PARTIAL qualification: chunking, budgets, prefix-preserving retention and staging reclaim implemented | `.26`/`.69` for truthful policy docs; retention owners retain acceptance |
| 8 | Archive-first doctor, safe repairs and restore | PARTIAL: actual plans, candidates, fingerprints, receipts and backups; damaged-owner-archive recovery unproven | `scohn`, `.2`, `.3`, `.4`, `.6` |
| 9 | Versioned streaming logical archive round trip | PARTIAL: v3 codec/schema bridges/extraction implemented; pinned-engine restore and PK-collation acceptance fail/skip | `.34`, `svhcf`, `p94yt` |
| 10 | Correct BM25/Boolean/phrase/wildcard/filter search | WORKING* basic lexical; full source implements standard grammar and bounded fuel through Quill | `search_metamorphic`, `.11`, `.68`, cold/open/query-specific beads |
| 11 | Self-healing derived lexical generations | PARTIAL: staged atomic publish, background repair and retained generations implemented | `h3jnv`, `.46`, `.48`, native publication `lndgg` |
| 12 | Default hybrid with explicit degradation | WORKING* missing-model fail-open and explicit semantic refusal | `.11`, `ds7uy.4`, `jyfuq` for complete model-backed lifecycle |
| 13 | Useful local native MiniLM retrieval with explicit acquisition | Implemented; prior scoped native evidence exists; UNPROVEN on this HEAD/run | `jyfuq.2`, `dl6ye`, `bzurv`, `.33` |
| 14 | Owner-backed progressive quality/fast search | PARTIAL architecture: private owner capability always false; ordinary CLI collapses progressive to single (`vector_index.rs:224`, `lib.rs:33219`) | `ds7uy.1`–`.5`, `jyfuq` |
| 15 | Immutable semantic generation adoption and continuous serving through ingest | PARTIAL: new retained/current/hydration APIs are library-only; ordinary loader refuses current pointer without owner | `ds7uy.1`–`.4`, `962e8` |
| 16 | Native ANN acceleration, truthful readiness and WAL/shard coverage | PARTIAL: exact fallback protects correctness; persistent semantic ANN disabled; `hnsw_ready=false` distinct from presence | `ds7uy.3.3`, `wfm4e`, `of9i7`, `fdvur`, `3rz35` |
| 17 | Warm daemon and retained service avoid repeated setup | WORKING* lexical reader reuse; actual authenticated native daemon/service code exists | `.32`, native lifecycle/owner acceptance, `o9ixv` |
| 18 | Bounded robot retrieval, interpretation echo and no silent substitution | WORKING* positive/error/fallback samples; broad contracts implemented | `.11`, `.68`, budget/large-archive owners |
| 19 | Stable robot envelopes, discovery, schemas and goldens | PARTIAL: many golden surfaces; archive lacks promised hint; assertion-only and introspection gaps remain | New `.85`, `eq69o`, `.69`, `.20` |
| 20 | Deterministic cited, private answer packs | WORKING* narrow extraction/privacy; full MAY/MUST reconciliation incomplete | `.20`; `ANSWER_PACKS_CONFORMANCE_MATRIX.md` is not a passing certificate |
| 21 | Evidence distinguishes landed, trusted, stale and failed attempts | PARTIAL: live correlation gives likely/unverified/stale; claim-specific trusted/failed proof remains outside ordinary correlation | Existing guided/trust and swarm evidence epics |
| 22 | Useful live swarm cockpit without taking ownership | PARTIAL: bounded Git/Beads/Mail/RCH and passive CASS/proof metadata readers exist; evidence is unverified, counts can be unknown | `.16`–`.18`, `oh96l`, `swarm-coordination-intelligence-gnrxb` |
| 23 | Guided plans execute and verify their advertised operations | PARTIAL: read-only proofs, support bundle and lexical repair adapters exist; other mutations explicitly unavailable (`guide_runner.rs:1020`) | `.19`, `guided-ops-repro-trust-5u82n` |
| 24 | Rich TUI navigation, ranking, filters, details, views and recording | Implemented, extensive tests; UNPROVEN current real-terminal/large-archive journeys | `.22`, residual `.54`, `.28` |
| 25 | Persistent CLI/TUI bookmarks | Implemented including Alt+E, BookmarkStore and populated tests; no fresh HEAD execution | `.26`/`.69` correct stale no-keybinding text |
| 26 | Correct shared CLI/TUI token analytics with bounded filters | PARTIAL qualification: accounting/query/rebuild layers real; whole-ledger accuracy and memory remain | `.21`, `xn5a4`, `xcqqa`, residual `rril3` |
| 27 | Safe portable HTML/generic export with optional encryption | PARTIAL: rendering/encryption real; direct raw source bypasses credential redaction | **New `.84`**, `.23`, `sd5o9` |
| 28 | Private filtered Pages export, password/recovery/revoke/rotate | Real implementation and prior native browser evidence; PARTIAL overall acceptance | `.23`, `70o8f`, key/export tests |
| 29 | Browser memory/offline/mobile/accessibility goals from Pages plan | PARTIAL: plaintext DB remains memory-only; 32 MiB prefetch bounds ciphertext, not total RSS; OPFS reuse unavailable (`database.js:7`) | `.23`, `.26` for historical disposition |
| 30 | Real multi-machine additive sync, provenance and source filtering | WORKING* mirror reingest only; real transport implemented; complete fleet proof separate | `.24`, `av59c`, source-specific issue owners |
| 31 | Scheduled freshness catches up without duplicate or unsafe work | Implemented timers, guards and durable state; UNPROVEN active fleet/macOS/large-archive convergence | `.24`, `962e8`, `synme`, `362yj`, `0m49m` |
| 32 | Concurrent storage improves whole indexing jobs | Engine mode on; single writer default; experimental parallel persist off. Benefit UNPROVEN | `.29`; do not claim enabled default parallel writers |
| 33 | Current, checksum-verified cross-platform installation/release | PARTIAL: DSR project configured; latest release lags HEAD and crates.io further lags | `.25`, `yrjna`, `5v57k`, `zztql`, platform/security owners |
| 34 | Honest performance and consistent current documentation | PARTIAL: meaningful measured fixes coexist with open whole-job costs and historical contradictions | `.26`, `.69`, `3zq55`, `.72`, `o9ixv`, `.11` |

Keyword scan found no committed `todo!()`/`unimplemented!()` sites under `src/`;
AST checks on the dispatch/search/guide entry paths also found none. This is a
bounded placeholder check, not a proof that every path works. The meaningful gaps
are demonstrated refusals, disabled integrations, measured costs and missing
acceptance, rather than the number of TODO comments.

### Newly uncovered gaps and specific existing gaps

**`.84` — raw-source export redaction parity, P1.** The installed-binary control
has redacted canonical search and canonical drill-down, yet raw HTML exports
reintroduce the same recognized password. Both generic `run_export` and
`run_export_html` prefer a readable local JSONL through the helpers around
`lib.rs:107246`; the raw preferences are around `:114055` and `:114701`.
HTML and Markdown probes include title/body/receipt coverage. The fix belongs at
the sharing boundary with the existing redactor, visible effective policy, and
an explicit sensitive raw-fidelity option if that workflow is retained. Preserve
source evidence and message identity. Narrowing the README alone does not fix it.

**`.85` — archive robot envelope, P2.** `main.rs:347` builds a separate four-field
error; `handle_fatal_error` at `:169` builds the advertised five-field error with
nullable `hint`. Use the shared serializer while preserving archive classification,
stderr-only failure and no-clobber behavior. A real missing-file negative must fail
against the current implementation and pass after the fix.

**Continue adapter parity remains under `cooow`.** Normal scans use the CASS
Continue adapter; watch/retry use FAD directly. Test identical incomplete,
backdated and excluded snapshots through all entry points. A registry count alone
cannot prove that the retry/completion policies agree.

**Documentation validation has a concrete parsing defect under existing `.26`.**
The fresh link-only run reports 82 findings. `check_links` extracts a path using a
pattern requiring a non-`#` first character; a fragment-only `[label](#anchor)`
therefore remains the entire original Markdown expression, fails the subsequent
`#` check, and gets tested as a nonexistent file. The retained log contains many
such historical-plan TOC links. Fix the parser with valid fragment/relative-link
controls and a genuinely missing-target negative; preserve real link checks and
classify the remaining findings. Do not delete the links or weaken the gate to
make this report green.

**Logical restore is not complete on the pin.** `scripts/gate.sh` retains named
bin-test skips for `svhcf` and `p94yt`. The former records fsqlite 0.4.12 rejecting
a valid WAL-materialized candidate's freelist during read-only snapshot integrity
checking; upstream fix `58ddae0de` is recorded but not adopted here. Passing a
selected gate would not clear those excluded populated probes.

**Semantic serving needs adoption, not another type layer.** Keep producer,
identity and owner guards. Connect immutable writers/readers, current-source
hydration, ordinary CLI/TUI/service and readiness as one unit, then qualify real
MiniLM/HNSW lifecycle, independent quality candidates, source mutation and restart.
Setting an owner flag true or regenerating graph fixtures would not do this.

**Resource efficiency remains a product issue.** The latest `3zq55` comment
contains actual Oct27 current-main observations on a synthetic 31k-session archive:
13.0–13.1 s / 5,318 MiB logical reads unchanged, 19.0 s / 8,718 MiB / 4.18 GiB
peak RSS after one append. These are earlier retained measurements, not this
session's benchmark. The same-day `w5137` live release A/B reports a fixed
quadratic mirror walk, 1,437.7 s versus 1,798.2 s full indexing, with main's peak
RSS about 0.6 GiB higher. A legitimate throughput gain does not erase that loss or
prove the process-wide budget. Attack the observed whole-job cost next.

### Shipped state and blockers

Live observations on Oct27: the GitHub release API reports
[`v0.10.0`](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.10.0)
published `2026-10-02T02:54:21Z`; the crates.io API reports
[`coding-agent-search 0.7.0`](https://crates.io/crates/coding-agent-search/0.7.0).
The release tag's `Cargo.lock` has fsqlite/core `0.4.6`; reviewed main pins the
uniform `0.4.12` family. Current runtime qualification notes for the newer family
are useful scope-specific evidence, and do not prove repair of damaged archives.
The FAD git revision still prevents publishing this manifest directly to crates.io.

DSR is configured under `coding_agent_session_search`, with Linux amd64/arm64,
macOS arm64 and Windows amd64 targets. `dsr health all` reports Linux and Windows
healthy; macOS has 14.58 GiB free and fails the existing 20 GiB admission floor.
The alias `cass` is not registered; its initial lookup failure is not evidence of
a missing project config. No release was built or published in this assessment.

The Oct26 `browser-e2e-lane-bnueb` closeout records actual macOS browser execution
at `640ac09c`: 24 encryption + 39 HTML + 38 Pages + 23 CDN tests passed, with two
explicit WebKit skips. This supersedes the earlier claim that the browser lane
never runs. It does not establish current HEAD, Windows, mobile/accessibility,
complete offline reload/search, large-memory or publication-failure qualification.
The new 51-test Node run is additional narrow evidence, not a replacement.

Strict UBS (`5v57k`), exact-current-source acceptance, remaining platform cases,
the known storage probes and semantic/resource integration still constrain release
claims. `release-bar-decision-hutyl` is an existing owner decision, not an approved
exception. This assessment changes no release gate and infers no approval from
time elapsed, a prior release's known issues or a documentation edit.

### Work graph and the five reality-check answers

`br doctor` initially found DB/JSONL count and ID drift: 126 durable issues were
missing from the local materialization. `br sync --import-only --json` succeeded
with 126 created / 137 updated / 0 removed. Subsequent sync status reports 2,465
exportable DB issues and 2,465 unique JSONL IDs, no drift. The baseline was 2,298
closed, 82 open, 54 in progress, 30 blocked, one tombstone. No database or recovery
artifact was rebuilt or deleted. Graph structure was held by one reservation owner.

All 34 live open GitHub issues had at least one corresponding durable bead when
matching issue identifiers in titles/descriptions. Some have only closed fixes
plus a release-delivery obligation; that is not proof the reporter is fixed in the
installed release. No unsolicited public comments or issue closures were made.

1. **Working:** the actual ingestion/canonical/lexical/fallback/pack/drill-down/
   retained-service path passed on isolated synthetic data with the installed
   executable. TUI, analytics, repair, native semantic, encryption and Pages have
   substantial reachable implementations and prior scoped proof. They are not all
   freshly qualified in this run.
2. **Missing or incomplete:** `.84`/`.85`, adapter consistency, current semantic
   serving adoption/progressive/native ANN, ongoing-ingest semantic availability,
   bounded whole-process work and complete platform/browser/archive acceptance.
3. **Blocking:** unfinished integration and specific engine/API behavior, measured
   scale costs, the unconfirmed RCH execution, macOS admission, strict UBS, FAD
   publication and the explicit owner release decision. These are distinct kinds
   of blockers and should not be hidden behind a single test percentage.
4. **Would finishing all existing open/in-progress beads finish the project? No,
   as written.** Before this pass two confirmed bugs had no dedicated leaf.
   Existing beads include superseded version upgrades, stale premises, owner
   decisions and broad proof obligations. Closing them mechanically could neither
   implement the new gaps nor produce a current release. After the additive
   `.84`/`.85` and refinements below, all identified *current* gaps have explicit
   ownership, but implementation plus their named acceptance and delivery are
   still required. Unknown defects cannot be ruled out by graph coverage.
5. **Uncovered goals:** `.84` and `.85` are now self-contained bug leaves with
   negative unit/E2E assertions. Historical optional inline field queries, search
   context and dynamic completions lack specific leaves; `.26` now explicitly owns
   their adopt/defer/supersede decision. They have not been silently discarded or
   invented as new urgent feature commitments.

### Bridge plan, ambition and refinement

Use the existing `2l1b0` campaign and existing subsystem owners. Two new leaves
are sufficient; another broad epic would duplicate already-owned work.

| Order | Concrete outcome and implementation scope | Named acceptance / negative | Dependencies and ownership |
|---|---|---|---|
| 1 | Close raw-export secret bypass across generic/HTML source and archive paths; reuse shared redactor and expose policy | `export_redaction_parity`: recognized title/body/tool/metadata/receipt secret absent by default; explicit raw mode visibly sensitive; benign near-miss and identity preserved; encrypted payload checked after decrypt | `.84`; release `.25` blocks on it |
| 2 | Share delegated archive error serialization | `delegated_archive_error_schema`: missing-file exit14, empty stdout, one stderr object, all five fields; malformed-wire and ordinary error controls | `.85`, shared robot discovery/schema owner |
| 3 | Make connector entry points use equivalent completeness policy | Same incomplete/backdated/excluded Continue source through batch/watch/quarantine; current direct-upstream route fails the negative | `cooow`; keep active owner |
| 4 | Adopt complete semantic serving generation and owner lifecycle | True independent quality hit outside fast topK, wrong-producer refusal, restart/ingest/edit/delete/atomic rollover, real native ANN exact oracle across shards/WAL | `ds7uy.*`, `962e8`, `jyfuq.2`, `wfm4e`; guard removal only after proof |
| 5 | Reduce measured whole-index/read/open/memory cost | Current-source real-corpus job profiles, actual RSS/IO, bounded progress and restart; include worst-loss workloads and A/A controls | `.46`/`.72`, `3zq55`, `xn5a4`, `xcqqa`, `o9ixv`; no microbench-as-job claim |
| 6 | Complete restore, fleet, terminal, analytics and browser user journeys | Populated named probes including skipped restore case, real source sync, unchanged repeats, real terminal, native model and scoped platform/browser cases | `.11`, `.21`–`.24`, `.34`, `svhcf`, `p94yt`; preserve skips as non-passes |
| 7 | Qualify and deliver one exact candidate under current approved bar | Source/lock/binary/model/corpus identities; nonzero terminal test counts; full required STAGE receipts, strict UBS, DSR artifact checks and channel observations | `.25`, `5v57k`, `yrjna`, `zztql`, existing owner decisions; no Actions |
| 8 | Keep current claims usable and historical ambition explicit | Source-backed connector/flag/exit/env examples; retention/ANN/bookmark correction; explicit optional-feature disposition | `.26`/`.69`; docs alone never close feature/acceptance work |

Each implementation leaf includes its own tests and sanitized detailed E2E logs.
The suite's ban on scope-splitting takes precedence over an older skill example
that creates a separate test closure for the same unit of work. No test or gate
is weakened, no failing control is removed, and no golden is regenerated here.

Three ambition rounds improved this bridge before its final graph review:

1. **Cross-surface behavior:** expanded the privacy finding from an unindexed raw
   file to indexed-versus-exported parity, both export families, receipts and
   decrypted output; preserved intentional raw fidelity through explicit policy.
2. **Actual serving and whole-job value:** targeted owner-backed semantic adoption,
   quality candidates independent of fast topK, and observed read/RSS losses;
   rejected another scaffolding layer or generic benchmark matrix as progress.
3. **Recovery and delivery:** bound each claim to source/executable/corpus identity,
   included old/new publication and native ownership negatives, corrected real
   browser/DSR progress, and retained the release bar rather than relaxing it.

The frozen skill bead-generation and refinement checklists were applied to the
existing graph, using `br` for every mutation. Five refinement passes then checked:

1. **Coverage:** both new defects get leaves; historical optional promises get an
   explicit `.26` disposition rather than an implicit deletion or duplicate epic.
2. **Behavior preservation:** source/canonical bytes, coordinates, raw-fidelity
   workflow, all export formats and current archive wire contracts remain covered.
3. **Falsifiability:** each new leaf specifies a populated negative the current
   implementation fails, benign controls and exact-source E2E receipts; old
   16 MiB stack prose is superseded by 128 MiB in new acceptance.
4. **Dependencies and proof class:** `.25` depends on `.84`; `.85` remains a
   distinct robot-contract fix; existing owners retain their statuses. Real
   browser evidence and narrow Node/installed-binary evidence remain separate.
5. **Convergence:** reviewed both complete new descriptions and existing-owner
   comments against the full checklist and cycle/triage output. No further
   nonduplicating implementation leaf or dependency correction was identified.

Existing `.26`, `cooow`, `.25` and `ds7uy.3` received concrete evidence/acceptance
comments; `.26` also received an additive named acceptance criterion for the
observed link-validator defect. No peer task was claimed or closed. Final
`br dep cycles`, including archived issues, reports zero cycles. `bv` triage and
planning succeed: 84 open / 54 in progress / 30 blocked, 116 actionable. `bv`
insights skips its own cycle enumeration above 2,000 nodes, so its empty array
is not the cycle proof; the explicit `br` check is. The forecast has low
confidence and is not a delivery commitment. No existing issue ID or status was
removed or changed. Final graph/sync receipts are recorded in the private
evidence directory and the assessment closeout comment.

---

## Assessment: 2026-10-22 (RainyBluff)

Scope: `origin/main` `9988ceec` (701 commits after the `v0.10.0` tag, Cargo version still
0.10.0). Read all 1,380 lines of `AGENTS.md` and all 3,867 lines of `README.md` on that
commit, the 10-14 section below, and the open bead and GitHub ledgers (2,446 beads; 34 open
issues). Evidence is from this session: rch gates b108j, b111, b113b, b115, b116 and b117e,
native Windows runs on wlap, and read-only `cass` 0.10.0 probes of this host's own archive
(`health`, `status`, `storage`, `schedule status`). Nothing in the live archive was written.

### Headline

The code on `main` is far ahead of what users run, and the gap is widening. Seven hundred
commits of fixes have not shipped: the fsqlite 0.4.12 b-tree corruption fix (bd-obwsy;
shipped v0.10.0 runs the affected 0.4.6), the truthful freshness model, raw-mirror growth
and quarantine retention, query fuel, timeline OOM, and the Windows OpenCode export abort.
The release is not blocked by one bug. It is blocked by a dependency graph that cannot
converge: `yrjna` waits on a strict UBS pass over ~720K lines (`5v57k`), and `2l1b0.25`
waits on about fifteen "prove X end to end at scale" beads, including repairing the owner's
damaged archive (`scohn`). crates.io is further behind still: it serves 0.7.0 from
2026-08-25, and cass cannot publish there while FAD is a git dependency (`zztql`).

The clearest evidence is this host. Installed cass 0.10.0 reports the archive `healthy`,
`fresh: true`, `stale: false`, while `last_indexed_at` is 2026-10-07: 15.8 days old against a
30-minute threshold. Every agent search here has silently missed two weeks of sessions. No
scheduler is installed. The last background catch-up (2026-10-13) refused to start because a
full lexical rebuild required 196.5 GB free and 37 GB was available. After that, the
freshness bug (fixed on main as `2l1b0.81`, unreleased) reported `fresh`, so stale-on-read
never tried again. The data dir is 220 GB: canonical DB 42.7 GB, lexical index 81.1 GB (live
generation 41 GB, plus 34 GB in four failed-rebuild quarantine bundles that 0.10.0 never
retired), raw mirror 86.5 GB. Releasing `main` alone would not heal this host. Main's rule
still charges `2 x DB + 2 x live lexical` (about 167 GB) for a lexical-only repair of an
unchanged archive. Only 81 GB is free, or 115 GB after pruning the quarantines.

### Vision checklist (README/AGENTS promises against `main`, with the shipped 0.10.0 where it differs)

| # | Promise | Status | Evidence |
|---|---|---|---|
| 1 | Discover and ingest 34 connectors, never modify sources | WORKING | capabilities golden lists 34; connector suites green in b100/b104b |
| 2 | SQLite is a durable source of truth | PARTIAL | main pins fsqlite 0.4.12 (bd-obwsy fix); shipped 0.10.0 runs the affected 0.4.6; #391 recurring b-tree damage open (`2l1b0.2` blocked); owner archive integrity open (`scohn`, `2l1b0.73`) |
| 3 | Lexical search is fast, required and self-healing | WORKING small/medium; BROKEN at scale | gates green on fixtures; on a 9.7M-document archive the only repair path cannot start (headroom rule, quarantine growth); about two thirds of the 34 open issues are large-archive wedges, OOM, fragmentation or corruption |
| 4 | Hybrid default with opt-in local semantic, truthful fail-open | PARTIAL | fail-open and metadata work; two-tier progressive is inactive (README now says so); native ANN admission over WAL updates red (`ds7uy.3.3`, 2 lib tests); 2,000-char prefix regression red (gh470 vs #458, `962e8`) |
| 5 | Index stays fresh automatically, freshness surfaces are truthful | REGRESSED on 0.10.0; PARTIAL on main | host above; on main the daemon timer was silently throttled to the 300 s read-path cadence and disabled entirely by `CASS_AUTO_REFRESH_SOURCE_SCAN_SECS=0` (107fabfa); two #515 daemon tests red on plain main (b115). Fixed in this pass, see below. Active-writer deferrals were silent in `index --json` (fixed in this pass) |
| 6 | Stable robot contracts, no silent substitution | WORKING | goldens 68/0, cli_robot 329/0, search_metamorphic green |
| 7 | Responsive TUI at scale | UNPROVEN at scale | `2l1b0.22` open; snapshot suites only |
| 8 | Multi-machine sources with provenance | WORKING in fixtures; UNPROVEN live | e2e_sources 121/0; live harness exists, `2l1b0.24` and `av59c` open |
| 9 | HTML/Markdown/Pages/archive export with encryption | WORKING in Rust; browser side UNPROVEN | html_export_integration 28/0, pages_bundle green; the Playwright suites have had no runner since the GitHub Actions removal (no bead until this pass) |
| 10 | Archive-first doctor and recovery | WORKING on fixtures | cli_doctor 117/0; a real damaged archive (owner's) is still unrepaired |
| 11 | Linux, macOS and Windows | PARTIAL | Windows: 26 native lib/integration failures (`if17v`), publish blocked by an open reader (`lndgg`), `v107y`; macOS: proof host's disk is 98% full, `d32jo` and `2l1b0.36` open |
| 12 | Installer, Homebrew, Scoop, GitHub releases, crates.io, self-update | STALLED | last release v0.10.0 (2026-10-02); crates.io 0.7.0; dsr release path works (`yviq2`'s cass part) but the release beads cannot close |
| 13 | Local-only, index-time secret redaction | WORKING | redaction suites green |
| 14 | Analytics, timeline, pack, bookmarks, swarm | WORKING | timeline OOM fixed on main (unreleased) |
| 15 | Truthful README | PARTIAL | glibc floor stated as 2.38 at line 592 (installer and line 2987 say 2.28); 21 migrations documented, code has 22 (`forgotten_sources`); a CI pipeline section and Codecov badge describe Actions jobs that no longer exist; crates.io paragraph says `cargo install` builds the current line (it builds 0.7.0); connector diagrams show 20 of 34 |

### The five questions

1. **What works now.** On Linux, small and medium archives, the core loop works end to end:
   discovery across 34 connectors, canonical SQLite, Quill lexical search, robot contracts,
   view/expand/pack, export, doctor on fixtures, multi-machine sync on fixtures. The
   release-qualification sweeps this week (gates b100, b104b, b108j, b113b and b117e) ran
   fmt, clippy, the full lib suite and 40 distinct integration targets of about 320. Remaining
   reds on main: the two ANN `wal_tests` (`ds7uy.3.3`),
   `gh470_prefix_checkpoint` (`962e8`), and four #515 daemon process tests (two fixed in this
   pass).
2. **What does not work.** (a) Users run a 20-day-old release that lacks a data-integrity fix
   and reports a stale index as fresh. (b) Self-healing fails on large archives: repair needs
   more free disk than such hosts have, quarantine bundles accumulate, and most open GitHub
   issues are scale failures. (c) Semantic acceleration is partial (ANN admission, the gh470
   prefix regression, two-tier inactive). (d) Windows and macOS proof is incomplete, and the
   browser suites never run.
3. **What blocks.** (1) Release governance: the release depends on beads that cannot finish
   on any short horizon (strict UBS over the monoliths, at-scale proof beads, owner-archive
   repair). This is an owner decision. (2) Two owner design calls hold red tests on main: ANN
   admission (`ds7uy.3.3`) and gh470 versus #458 retention (`962e8`). (3) Proof
   infrastructure: rch workers refill their disks within a day (the rch GC never collects
   staging source copies; 427 GB reclaimed by hand on 10-21 and 10-22), the macOS host is
   full, and there is no browser lane. (4) The FAD git pin blocks crates.io.
4. **Would finishing every open and in-progress bead close the gap? No.** The 10-14 answer
   ("YES") does not hold. The set is not finishable as written: 58 of 71 in-progress beads
   have not been touched since before 2026-10-01, and some are obsolete (`90y8u` upgrade to
   fsqlite 0.3.18, `91njy` release v0.8.0, `hyydu` adopt 0.4.4). Others are unbounded (strict
   UBS on 720K lines, `scohn`). Finishing them also would not ship anything, because no bead
   owns cutting a release from the current main under a bar it can meet. And four gaps found
   in this pass had no bead.
5. **Goals no bead covered (beads filed in this pass, children of `2l1b0`, label
   `reality-check-20261022`):**
   - `lexical-repair-headroom-nw61n` (P1): the preflight charges `2 x DB` even when the DB is
     unchanged, so self-healing cannot start on large archives. The `lexical x 2` term is
     real (trj consumed about 115 GB for a 25 GB DB in July,
     `index-rebuild-headroom-underestimate-2mm1t`), so the bead measures the real peak per
     path before changing the rule.
   - `release-bar-decision-hutyl` (P0, question for the owner): ship 0.11.0 under a "no
     worse than 0.10.0" bar with a known-issues list, and move certification beads off the
     release path. Related to `yrjna`.
   - `browser-e2e-lane-bnueb` (P2): a runner for the Playwright HTML-export and Pages suites
     on a dsr host. `2l1b0.23` now depends on it.
   - `readme-truth-20261022-9h3pj` (P2): the README claims in row 15.

### Bridge plan (ordered by user value per unit of work)

1. **Ship what is done.** Land the fixes from this pass. The owner decides the release bar.
   Then cut 0.11.0 through dsr with a known-issues list, then a scheduled release train.
   Every week without a release leaves the fixes on main undelivered and users on known-bad
   code.
2. **Make self-healing possible at scale.** Measure the real peak disk of a lexical-only
   rebuild (staged shards plus merge output) on a fixture large enough to show it. Charge
   that for a lexical-only repair of an unchanged archive. Keep the DB term for paths that
   rewrite the DB (migration repair, `--full` rescans). Count retained quarantine bundles
   and retire superseded ones. Negative test: a `--full` rescan still charges the DB term.
   Longer term, repair the broken generation segment by segment instead of rebuilding it,
   which removes the 2x requirement altogether.
3. **Settle the owner calls that hold reds on main:** `ds7uy.3.3` (proposal posted) and
   `962e8`.
4. **Truth surfaces:** freshness (in the release), active-writer deferral reporting (this
   pass), README truth pass (this pass).
5. **Proof lanes:** a browser runner on a dsr host; the Windows baseline (`if17v`, `lndgg`);
   room on the macOS host; an rch staging-copy retention policy for the fleet (an infra
   change outside this repo, owner call).
6. **Ledger hygiene (`2l1b0.30`):** release stale claims and close obsolete beads with
   evidence, so `br ready` and `bv` point at real work.

### What this pass changed on main (receipts in the commits)

- `cass index --json` reports `indexing_stats.deferred_active_sources` and a sample when a
  writer kept sources from being read (h3jnv / #530). It also adds the reporter's exact
  timer sequence as a regression test: on current main, `--full --force-rebuild`, then
  `--watch-once` over 11 new sessions, then a plain index appends without rebuilding.
- The archive lease path drops std's `\\?\` spelling on Windows (txc9h; native Windows red
  then green on wlap).
- The resident daemon's periodic index runs at its own interval again (#514/#515: two red
  daemon process tests go green).
- The `franken_sync` regression test shows that a cancelled `BEGIN IMMEDIATE` waiting on
  another writer returns `Abort` promptly. This rules out the engine as the cause of the
  remaining #515 migration-shutdown failure.

Limits: no macOS run (the host is full), no browser run, no run on a damaged archive, no
full 320-target sweep (fleet disks), and the host archive was observed, not repaired.

## Assessment: 2026-10-14 (Antigravity)

Scope: local `main` at `80ed2952` (~461 commits ahead of the `v0.10.0` release tag from 2026-10-01; ~711 commits ahead of the 2026-09-23 reality check at `71759163`). Read all 1,371 lines of `AGENTS.md`, all 472 lines of `/data/projects/AGENTS.md`, all 3,840 lines of `README.md`, `/home/ubuntu/.codex/skills/reality-check-for-project/SKILL.md` (plus its 5 reference files), `/home/ubuntu/.codex/skills/just-say-no-to-process-porn-and-ceremony/SKILL.md`, `/home/ubuntu/.codex/skills/idea-wizard/SKILL.md`, `/home/ubuntu/.codex/skills/modes-of-reasoning-project-analysis/SKILL.md`, and `/home/ubuntu/.codex/skills/brennerbot-with-ntm/SKILL.md`. Cross-referenced all **39 open GitHub issues** (`#526` down to `#320`), all `coding_agent_session_search-2l1b0.*` campaign beads (`.1` through `.79`), and all tracker issues (`bv --robot-triage`). Executed read-only runtime probes against the host archive (`/home/ubuntu/.local/share/coding-agent-search`, `cass 0.10.0`), `./scripts/validate_docs.sh --readme --links --keys --env`, and `./scripts/gate.sh --verify-fmt`. No live archive was written and no file was deleted.

### Codebase Scale & Tracker Snapshot at `80ed2952`

| Metric | 2026-09-01 | 2026-09-23 (`71759163`) | 2026-10-14 (`80ed2952`) |
|---|---:|---:|---:|
| Rust source files in `src/` | 245 | 491 | **515** |
| Lines of Rust in `src/` | 541,218 | ~664,000 | **697,114** |
| Top 4 monoliths (`src/lib.rs` + `src/indexer/mod.rs` + `src/ui/app.rs` + `src/storage/sqlite.rs`) | ~235,000 | ~281,000 | **295,062** (`127,705` + `72,580` + `48,647` + `46,130`) |
| Total `#[test]` functions (`src/` + `tests/`) | ~10,800 | ~12,100 | **12,803** (`9,081` unit + `3,722` integration; `136` `#[ignore]`) |
| SQLite `CURRENT_SCHEMA_VERSION` (`src/storage/sqlite.rs:4100`) | 19 | 22 | **22** |
| Connectors in `capabilities.json.golden` | 31 | 32 | **35** (added `continue`, `cline`, `qwen`, `pi_durable`) |
| Tracker issues (`bv --robot-triage`) | ~2,000 | ~2,280 | **2,424** total (`2,179` closed, `245` unfinished: `86` open, `126` in_progress, `33` blocked, `183` actionable, `0` cycles) |

---

### Phase 1: The Five Core Reality-Check Questions ("Where Are We REALLY?")

#### 1. What specifically IS working right now?

1. **Discovery & Canonical Ingestion across 35 Agent Connectors**:
   - `franken-agent-detection` + CASS local connectors enumerate **35 connectors** in `tests/golden/robot/capabilities.json.golden` (`aider`, `amp`, `antigravity`, `chatgpt`, `claude_code`, `clawdbot`, `cline`, `codex`, `Continue`, `copilot`, `copilot_cli`, `crush`, `cursor`, `devin`, `droid`, `factory`, `gemini`, `goose`, `hermes`, `kimi`, `kilo_code`, `kilocode`, `mastra`, `omp`, `openclaw`, `opencode`, `pi_agent`, `pi_agent_rust`, `pi_durable`, `plandex`, `qoder`, `qwen`, `shelley`, `vibe`, `windsurf`).
   - Recent connector/ingest/mirror fixes landed and verified on `main`:
     - **GH #520 (`m520r`, `mx520`, `mxr52` CLOSED)**: Codex JSONL lines where `payload` is not a JSON object skip cleanly instead of aborting the whole file; remote mirrors scan only connectors owning the configured store path (`e39ce023`), and `cass dedup` cleans prior misattributed rows (`9f20b562`).
     - **GH #521 (`sbs52`, `t521m`, `tsp52` CLOSED)**: `provenance_summary` is stripped before parsing `source_bytes_seen` so incremental runs no longer re-parse multi-GB Claude/Codex transcripts from byte 0.
     - **GH #522 (`rm522` CLOSED)**: JSONL/NDJSON transcripts capture as 256 KiB `fixed_chunks_v1` (`4c10a2a1`, 22.8x less mirror growth on append workloads), enforce capture budgets (`bc585092`, `924eb886`), and support `cass mirror prune --keep-latest N` (`99ad3d01`).
     - **GH #523 (`rq523`, `ms523` CLOSED)**: Failed lexical rebuilds retain only the newest bundle (`9c258ea3`), and `cass mirror prune` reclaims abandoned `raw-mirror/v1/tmp/capture.*` staging directories (`0fa1be65`, `d4b74ebe`).
     - **GH #511 (`b7rhs`, `mt90z` CLOSED)**, **GH #513 (`codex-zstd`)**, **GH #504 (`opencode` v2)**, **GH #500 (`claude_code` mid-turn prompts)**, **GH #486 (`CASS_EXCLUDE_PATHS`)**.
2. **Canonical SQLite v22 + Quill v9 Lexical Search & Filtering**:
   - On the live host archive (`/home/ubuntu/.local/share/coding-agent-search`), `cass health --json` and `cass status --json` verify a healthy `1.64 GB` database (`wal_bytes = 0`), `944` conversations, `490,786` messages, `14` Quill segments (`live_docs = 490,786`), `checkpoint.completed = true`, and `semantic.status = "ready"` (`hash-384`, `535,442` docs).
   - **17 of the 31 `2l1b0.49`–`.79` beads from the 2026-09-23 reality check are now CLOSED** with real implementations:
     - **GH #499 (`2l1b0.49` CLOSED)**: `frankensearch =0.7.1` / `frankensearch-quill =0.4.1` restores live-doc scoring on tombstoned segments with date/agent filters.
     - **Boolean Query Precedence (`2l1b0.52` CLOSED)**: Standard `NOT > AND > OR` precedence and parenthesized groups work in Quill (`frankensearch#56`).
     - **Date Filter Validation & Inclusive `--until` (`2l1b0.64` CLOSED)**: `--since`/`--until` reject malformed dates and treat `YYYY-MM-DD` `--until` as end-of-day across `search`, `timeline`, and `sessions`.
     - **Privacy / Erasure Convergence (`2l1b0.50`, `2l1b0.78`, `2l1b0.79` CLOSED)**: `cass forget --apply` purges hits from Quill lexical segments, semantic vector assets recover cleanly after `forget`, and incremental indexing converges lexical doc counts after out-of-band DB deletions.
     - **TUI & CLI Polish (`2l1b0.51`, `.53`, `.56`, `.57`, `.59`, `.60`, `.66`, `.70`, `.71`, `.75`, `.76`, `.77` CLOSED)**: `status --jsn` typo routing, TUI F12 ranking cycle, TUI first-run background index without blocking update prompt, `CASS_DB_PATH` honoring, Pages share profiles & `pages_bundle` 64/64 green, TUI semantic auto-enablement, and OMP analytics fresh writer.
3. **Delegated Subcommands & Operational Repair Tooling**:
   - `cass` dispatches `serve`, `archive`, `codex-layout-migrate`, `lexical-quarantine`, and `semantic-reclaim` (`src/main.rs:309-316`), backed by companion binaries (`cass-codex-layout-migrate`, `cass-lexical-quarantine`, `cass-semantic-reclaim`).
4. **Documentation Validation (`scripts/validate_docs.sh`)**:
   - `./scripts/validate_docs.sh --readme --links --keys --env` passes with **0 errors**: all 38 CLI subcommands exist in `README.md`, all 29 internal anchor links resolve, all 52 keybindings match `src/ui/input.rs`, and all 70 `README.md` environment variables are read in `src/`.

#### 2. What is NOT working or not yet implemented?

1. **[NEW DISCOVERY — `NO_BEAD` -> `coding_agent_session_search-2l1b0.81`] `--stale-threshold` and `index.stale` / `index.fresh` Are Permanently Suppressed in `cass status` and `cass health`**:
   - **Live Evidence**: On `/home/ubuntu/.local/share/coding-agent-search`, `cass status --json` and `cass health --json` report `last_indexed_at: "2026-10-13T21:25:42.429+00:00"`, `age_seconds: 14741` (4.1 hours), `stale_threshold_seconds: 1800` (30 minutes), yet **`stale: false, fresh: true, healthy: true`** (and `cass health --stale-threshold 60` exits `0`).
   - **Exact Code Cause (`src/search/asset_state.rs:1660-1662`)**:
     ```rust
     let age_exceeds_threshold = age_seconds.is_some_and(|age| age > stale_threshold_seconds);
     let age_stale_covered_by_fingerprint =
         age_seconds.is_some() && fingerprint_matches == Some(true);
     let age_stale = age_exceeds_threshold && !age_stale_covered_by_fingerprint;
     ```
     Commit `6a398535` (for GH #452) added `age_stale_covered_by_fingerprint` so `cass search` wouldn't treat an old-but-matching index as `status = "stale"` and trigger an expensive archive-scale lexical rebuild. However, because coding agents write to external session files (`~/.claude`, `~/.codex`, etc.) and never write directly to `agent_search.db`, `current_db_fingerprint` (`content-v1:<convs>:<max_conv_id>:<max_msg_id>`) **always** equals `checkpoint_fingerprint` after any completed `cass index` run until the next `cass index` run. Therefore `fingerprint_matches` is always `Some(true)`, `age_stale_covered_by_fingerprint` is always `true`, and `age_stale` is **permanently `false`**.
   - **Why `synme` (GH #514, commit `107fabfa`) Only Masked Part of the Bug**: `107fabfa` added a 5-minute `source-scan-overdue` check in `src/indexer/background_refresh.rs:133-155` so `search`/`pack`/TUI still trigger background scans, but its author assumed `asset_state.rs` still flipped `stale: true` at 30 minutes (`DEFAULT_STALE_THRESHOLD_SECS = 1800`). Because `asset_state.rs:1660-1662` suppresses `age_stale`, `cass health` and `cass status` lie about archive freshness regardless of `--stale-threshold`.
2. **[NEW DISCOVERY — `NO_BEAD` -> `coding_agent_session_search-2l1b0.80`] GH #526 Has Zero Beads and `STAGE=fmt` Fails on 91 Files on `main`**:
   - Across all 39 open GitHub issues, **GH #526** (`main fails cargo fmt --check since 2026-10-07 (b2c46a14); 74e02103 left engine.rs unparseable until c119b343`) was the **only** issue with no bead in `.beads/issues.jsonl`.
   - `./scripts/gate.sh --verify-fmt` fails (`STAGE=fmt EXIT=1`) on **91 Rust files** on `main`.
3. **[ROOT-CAUSE DIAGNOSED — `coding_agent_session_search-dcp01`] Three `tests/cli_doctor.rs` Cleanup Tests Fail 0/3 on `main` Because `publish_staged_lexical_index` Backs Up an Empty Directory Created by `index_dir()`**:
   - In `publish_staged_lexical_index` (`src/indexer/mod.rs:24622`), the fast-path `if !index_path.exists()` never triggers in production because `index_dir(data_dir)` (`frankensearch::index_dir`) calls `std::fs::create_dir_all(&index_path)` at the start of every index run!
   - When `1f5d790e` (`at3us`) routed first-run empty-archive indexing through `rebuild_tantivy_from_db` -> `publish_staged_lexical_index`, `seed_healthy_empty_index` in `tests/cli_doctor.rs` began swapping the empty `index_path` directory into `.lexical-publish-backups/v9-quill.<ts>.bak` as a bogus retained backup, breaking all three `cli_doctor.rs` cleanup tests (`6426`, `7420`, `7647`).
4. **Active Regressions & High-Severity Open Bugs on `main`**:
   - **Watch-Once Single-File Regression (GH #524 / `wo524` P0 `in_progress`)**: Commit `7cfebcb7` (`discover_watch_once_source_paths`) treats single-file `--watch-once <codex-rollout.jsonl>` paths as directories (`ENOTDIR`, exit 9) and passes `.jsonl` files to Shelley/Devin SQLite readers.
   - **Continue Connector Watch/HTML Wiring (`ctd01` P1 `in_progress`)**: FAD's `continue` connector lacks a `ConnectorKind` arm and HTML export identity.
   - **Timeout-Killed Index Runs Quarantine Partial Lexical Candidate (GH #523 / `rr523` P1 `in_progress`)**.
   - **Semantic Checkpoint Memory Spike (GH #525 / `se525` P1 `in_progress`)**: Commits `bb60f352`..`80ed2952` landed bounded message-ID delta hydration and paged replay-cache inventory streaming; awaiting final gate verification.
   - **Windows OpenCode Live-WAL Export Crash (GH #519 / `zjv7m` P1 `open`)** & **macOS `watch_startup` Wedge Under Daemon (GH #515 / `362yj` P1 `open`)**.
5. **Incomplete Architectural Milestones & Nearly-Wired Code**:
   - **Two-Tier Progressive Search (`ds7uy.3` P1 `in_progress`)**: `src/search/vector_index.rs:228` still hardcodes `owner_backed_progressive_reader: false`.
   - **Native ANN Over Retained WAL Updates (`ds7uy.3.3` P2 `open`) & Persistent Serve ANN (`3rz35` / `2l1b0.32`, GH #452)**.
   - **Five Production `sqlite3` CLI Fallbacks Remain in `src/storage/sqlite.rs` (`2l1b0.65`)** (awaiting upstream `frankensqlite` repair-mode open).
   - **Nearly-Complete Campaign Beads (`2l1b0.54`, `2l1b0.55`, `2l1b0.58`, `2l1b0.61`, `2l1b0.67`, `2l1b0.68`, `2l1b0.74`)**:
     - `2l1b0.67` (encrypted `export-html` title/metadata sealing) and `2l1b0.61` (Pages typed recovery key + non-reused slot IDs) and `2l1b0.74` (Windows lock contention) are already implemented and tested in Rust (`52394e56`, `2af63546`, `2d123091`, `d4a82574`, `b4521d34`), with only their final verification/closeout items remaining!
     - `2l1b0.55` has `F9` match mode and saved-view query persistence landed (`d05c29b3`), leaving only the `F7`/`Ctrl+Space` context-window consumer in `src/ui/app.rs`!
6. **[NEW DISCOVERY — `NO_BEAD` -> `coding_agent_session_search-2l1b0.82`] Documentation, Manifest & `robot-docs env` Drift**:
   - `Cargo.toml:167` and `build.rs:188-190` pin `franken-agent-detection` to `git = "https://github.com/Dicklesworthstone/franken_agent_detection", rev = "6b337ba434b72c65c6c5a748b7ec47885f840ac0"` (tracked for crates.io publication by `zztql`), contradicting `README.md:3678,3683` and `AGENTS.md:225`.
   - `codex-layout-migrate` and `lexical-quarantine` are shipped subcommands (`src/main.rs:309-316`), companion binaries (`Cargo.toml:45-61`), and robot capabilities (`capabilities.json.golden:119-120`), with **zero mentions** in `README.md` or `AGENTS.md`.
   - `capabilities.json.golden` lists **35 connectors**, while `AGENTS.md:721-754` lists 32.
   - `./scripts/validate_docs.sh --env` warns that **49 of 70 `README.md` environment variables** are absent from `cass robot-docs env`.

#### 3. What is blocking us from getting there?

1. **SQLite Family Qualification (`gi7k7` P0 `in_progress`, GH #516)**: Qualifying `fsqlite =0.4.12` across all 20 crates (`5309f842` recorded gate b35 evidence).
2. **Registry Publication of `franken-agent-detection` (`zztql` P2 `open`)**: Publishing FAD `>= 6b337ba4` to crates.io so `cass 0.11.0` can be published on crates.io and released via `/dsr`.
3. **Red Gate Baseline on `main` (`2l1b0.80` / GH #526, `dcp01`, `wo524`, `ctd01`)**: Restoring `STAGE=fmt`, the 3 `cli_doctor` cleanup tests (`dcp01`), `--watch-once` single-file handling (`wo524`), and `Continue` connector registry tests (`ctd01`).

#### 4. If we were to implement all open and in-progress beads, would we close the gap completely? Why or why not?

**With `coding_agent_session_search-2l1b0.80`, `coding_agent_session_search-2l1b0.81`, and `coding_agent_session_search-2l1b0.82` created and wired into `.beads/issues.jsonl`: YES.** Every vision requirement, open GitHub issue, and architectural invariant has an explicit bead owner with falsifiable acceptance criteria.

#### 5. What goals from the vision were NOT covered by ANY existing bead (and are now owned)?

| Uncovered Goal / Gap Found on `HEAD` | Code / Doc Location | Prior Bead Status | New Bead Created |
|---|---|---|---|
| **GH #526**: `cargo fmt --check` (`STAGE=fmt`) fails on 91 Rust files on `main` since `b2c46a14` | `scripts/gate.sh`, 91 `.rs` files | `NO_BEAD` | **`coding_agent_session_search-2l1b0.80`** (`P1`, bug; blocks `2l1b0.25`) |
| **Truthful `--stale-threshold` & `index.stale` / `index.fresh` in `status` and `health`**: `age_stale` is suppressed whenever `fingerprint_matches == Some(true)` | `src/search/asset_state.rs:1660-1662`, `src/indexer/background_refresh.rs:133-155` | `NO_BEAD` (`synme` closed with workaround in `background_refresh.rs`) | **`coding_agent_session_search-2l1b0.81`** (`P1`, bug; related to `synme`, blocks `2l1b0.25`) |
| **2026-10-14 Manifest, Subcommand, Connector & `robot-docs env` Parity**: FAD git pin vs registry doc claim, undocumented `codex-layout-migrate` & `lexical-quarantine`, 35 vs 32 connectors, 49 env vars missing from `cass robot-docs env` | `Cargo.toml:167`, `build.rs:188-190`, `src/main.rs:309-316`, `README.md`, `AGENTS.md`, `src/lib.rs` | `NO_BEAD` | **`coding_agent_session_search-2l1b0.82`** (`P2`, task; related to `2l1b0.26`, `2l1b0.69`, `zztql`, blocks `2l1b0.25`) |

---

### BrennerBot + Modes-of-Reasoning + Idea-Wizard High-Leverage Triage (30 -> Top 5 + Next 10)

Applying `/idea-wizard` (30 candidate improvements winnowed to Top 5 + Next 10), `/modes-of-reasoning-project-analysis` (independent claim audit separating code facts from stale tracker claims), and `/brennerbot-with-ntm`'s Hypothesis Opportunity Matrix (`Score = (MindChange × Falsifiability × OptionValue) / (Cost × Ambiguity × InfraRisk)`):

| Rank & Bead | Improvement / Gap-Closing Target | Mind | Falsify | Option | Cost | Ambig | Infra | Score | Execution Disposition |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| **#1 (`dcp01`)** | **Fix `publish_staged_lexical_index` empty-`index_path` backup bug (`src/indexer/mod.rs:24622`)**: `index_dir()` creates an empty `index_path` dir before first publish, causing `publish_staged_lexical_index` to retain a bogus empty backup in `.lexical-publish-backups/` and breaking 3 `tests/cli_doctor.rs` tests (`6426`, `7420`, `7647`). | 5 | 5 | 5 | 1 | 1 | 1 | **125.0** | **Execute immediately** (unclaimed `P1`, 15-line surgical fix + regression test). |
| **#2 (`2l1b0.81`)** | **Two-Edge Freshness Model in `src/search/asset_state.rs:1660-1662`**: Decouple **DB-to-Quill projection parity** (`status = "ready"` when `fingerprint_matches == Some(true)`) from **source-to-archive age staleness** (`stale = true, fresh = false` when `age_seconds > stale_threshold_seconds`), making `cass status` and `cass health --stale-threshold` truthful without triggering false inline Quill rebuilds on `cass search`. | 5 | 5 | 5 | 1 | 1 | 1 | **125.0** | **Execute immediately** (unclaimed `P1`, fixes broken `--stale-threshold` contract). |
| **#3 (`pc522`)** | **Wire `src/raw_mirror/store.rs` capture tests through production `policy::capture`**: Move `store.rs` fixture captures off the dead `#[cfg(test)]` pre-budget path onto `policy::capture` and add a negative over-budget `RawMirrorCapturePaused` test. | 4 | 5 | 4 | 1 | 1 | 1 | **80.0** | **Execute immediately** (unclaimed `P2`, eliminates test-only shadow path). |
| **#4 (`2l1b0.82`)** | **Complete `cass robot-docs env` (add 49 missing env vars) + document `codex-layout-migrate` & `lexical-quarantine` + 35 connectors**: Turn 49 `./scripts/validate_docs.sh --env` warnings into `0` warnings and bring `README.md`/`AGENTS.md` to 100% subcommand/connector parity. | 4 | 5 | 4 | 1 | 1 | 1 | **80.0** | **Execute immediately** (unclaimed `P2`, high user/agent discoverability). |
| **#5 (`2l1b0.55`)** | **Wire TUI `F7` / `Ctrl+Space` Context Window (`S`/`M`/`L`/`XL`) into results-list and detail preview rendering**: `context_window` is already tracked in `App` state and status bar (`src/ui/app.rs`), while `F9` match mode and saved-view query persistence already landed in `d05c29b3`. Wiring `context_window` into snippet/preview truncation completes `2l1b0.55`! | 4 | 5 | 4 | 2 | 1 | 1 | **40.0** | **Execute in this wave** (unclaimed `P2`, textbook "finished code waiting to be wired"). |

#### Next 10 High-Leverage Items (#6–#15)
6. **`2l1b0.80` (GH #526)**: Restore `cargo fmt --check` (`STAGE=fmt EXIT=0`) across all drifted Rust files on `main` without sweeping concurrent dirty files.
7. **`2l1b0.74`**: Close out Windows `fs2` -> `std::fs::File::try_lock` contention bead (code and native Windows `wlap` test runs already landed in `d4a82574`/`b4521d34` and verified in comment 2026-10-09; audit whether `fs2` can be removed from remaining non-contention sites or if bead is ready to close now that GH Actions is permanently off).
8. **`2l1b0.58`**: Add the held-operation-lock `exit 7` (`index-busy`) scenario to `doctor_exit_codes_match_the_advertised_contract` in `tests/cli_doctor.rs`.
9. **`ctd01` / `wo524` / `rr523` / `se525` / `gi7k7`**: Active swarm lane owned by `RainyBluff` (Continue connector wiring, watch-once Codex/Shelley fix, resumable lexical candidate after timeout, bounded semantic delta hydration, `fsqlite =0.4.12`).
10. **`2l1b0.67` & `2l1b0.61`**: Encrypted `export-html` title/metadata sealing (`52394e56`) and Pages typed recovery-key entry (`2d123091`) — Rust + Node worker crypto tests are already green on `main`; verify DOM/unlock flow without GitHub Actions so both beads can close cleanly.
11. **`ds7uy.3` & `ds7uy.3.3`**: Enable `owner_backed_progressive_reader: true` in `src/search/vector_index.rs:228` and native ANN over retained WAL updates.
12. **`3rz35` / `2l1b0.32` (GH #452)**: Keep a warm in-memory HNSW/Quill reader across requests in `cass serve`.
13. **`zjv7m` (GH #519) & `362yj` (GH #515)**: Windows live-WAL OpenCode export teardown and macOS `watch_startup` daemon handoff.
14. **`2l1b0.46` / `2l1b0.72` (GH #496 / #320)**: Enforce `CASS_MEMORY_BUDGET_MB` across lexical and semantic rebuilds on multi-million-message archives.
15. **`zztql` + `55wh0` + `5v57k` + `2l1b0.25`**: Publish `franken-agent-detection` to crates.io, run full `scripts/gate.sh` + `ubs`, and release `v0.11.0` via `/dsr`, closing 12+ already-fixed GitHub issues.

---

### Phase 2–5 Summary: Bridge Plan, 3 Ambition Rounds & 5 Plan-Space Refinement Passes

1. **Three Ambition Rounds (Failure-Class Elimination)**:
   - **Round 1 — Two-Edge Freshness Model (`2l1b0.81`)**: Separate **Edge 1: Source-to-DB Recency (`age_stale`)** (`age_seconds > stale_threshold_seconds`, which drives `cass status`, `cass health`, and `stale_reasons`) from **Edge 2: DB-to-Quill Projection Parity (`status == "ready"`)** (`fingerprint_matches == Some(true)`, which prevents `cass search` from running an expensive synchronous Quill rebuild when SQLite and Quill already match).
   - **Round 2 — Idempotent First-Publish & Self-Cleaning Staging (`dcp01`, `ms523`, `rr523`)**: Make `publish_staged_lexical_index` treat an existing-but-empty `index_path` directory as a first publish (`FirstPublish`) rather than retaining an empty backup in `.lexical-publish-backups/`.
   - **Round 3 — Self-Verifying Contract & Gate Parity Without GitHub Actions (`2l1b0.80`, `2l1b0.82`, `pc522`)**: Restore `STAGE=fmt EXIT=0`, route `store.rs` capture tests through `policy::capture`, and make `cass robot-docs env` cover 100% of the 70 `README.md` environment variables.
2. **Five Plan-Space Refinement Passes**:
   - **Pass 1 (Coverage & No Duplicate Ownership)**: Reconciled against `eba1d01c`..`80ed2952` (`ms523` closed, `zztql` and `ctd01` filed); verified zero overlap between `2l1b0.80`, `2l1b0.81`, `2l1b0.82`, `dcp01`, and `pc522`.
   - **Pass 2 (Exact Source & Proof-Class Anchors)**: Every bead cites exact `HEAD` (`80ed2952`) file and line numbers and specifies `[UNIT]`, `[INTEGRATION]`, or `[E2E]` proof classes.
   - **Pass 3 (Dependency Graph & Cycle Check)**: Wired `2l1b0.80`, `2l1b0.81`, and `2l1b0.82` under `coding_agent_session_search-2l1b0`, blocking `2l1b0.25`, and related to `synme`, `zztql`, `2l1b0.26`, and `2l1b0.69`; verified `0` cycles via `bv --robot-triage`.
   - **Pass 4 (Adversarial & Negative Test Specification)**: Every bead includes both positive and planted-negative assertions (e.g., `2l1b0.81` asserts `status: "ready"` AND `stale: true, fresh: false` when `age_seconds > stale_threshold` and `fingerprint_matches == Some(true)`).
   - **Pass 5 (Immediate Execution & Accretive Delivery)**: Transitioned directly from plan space into code execution per `/just-say-no-to-process-porn-and-ceremony`.

### Execution & Gate Receipt Summary (2026-10-14..2026-10-19 — 61 Beads Closed Across 5 Waves)

1. **Wave 1 (6 Beads Closed — `a5dc182d`, `52ebded3`, `c1b1b5d9`, `7958066a`, `54d9610f`, `eaf0a74c`, `6a241c92`; Receipts `/data/tmp/cass-gate.85vSx4`, `/data/tmp/cass-gate.VUDd59`)**:
   - **`2l1b0.80` (GH #526)**: Restored repo-wide `STAGE=fmt EXIT=0` across 90+ Rust files.
   - **`dcp01`**: Fixed `publish_staged_lexical_index` empty placeholder backup bug (`src/indexer/mod.rs`), restoring all 3 `tests/cli_doctor.rs` cleanup tests (`6426`, `7420`, `7647`).
   - **`2l1b0.81`**: Implemented the two-edge freshness model (`src/search/asset_state.rs`, `src/indexer/background_refresh.rs`) so `cass status` and `cass health` honor `--stale-threshold` and `CASS_STALE_THRESHOLD_SECS`.
   - **`pc522`**: Routed `src/raw_mirror/store.rs` capture tests through production `policy::capture` with over-budget pause diagnostics.
   - **`2l1b0.82`**: Added all 49 missing `README.md` environment variables to `cass robot-docs env` (`0` warnings in `./scripts/validate_docs.sh --env`), documented `codex-layout-migrate` and `lexical-quarantine`, and aligned 35 connectors in `AGENTS.md`/`README.md`.
   - **`2l1b0.55`**: Wired TUI `F7`/`Ctrl+Space` context-window (`S`/`M`/`L`/`XL`) geometry, `F9` match mode, and saved-view query persistence (`src/ui/app.rs`).
2. **Wave 2 (5 Beads Closed — `7ccfd590`, `8cb5815f`, `034b7f80`, `c0728653`; Receipts `/data/tmp/cass-gate.PUz1Fg`, `/data/tmp/cass-gate.0hOdkt`)**:
   - **`2l1b0.74`**: Closed Windows `fs2` -> `std::fs::File::try_lock` lock-contention migration across indexer, doctor, raw-mirror, Pages, and logical-archive locks.
   - **`2l1b0.58`**: Added the held-operation-lock `exit 7` (`index-busy`) scenario to `doctor_exit_codes_match_the_advertised_contract` (`tests/cli_doctor.rs`) and verified exit-code 4/6/8/70 contracts.
   - **`2l1b0.67`**: Verified encrypted `export-html` sealed title/header/metadata and added Node WebCrypto AES-256-GCM unlock verification in `test_encrypted_export_hides_topic_in_default_name_and_html` (`tests/html_export_integration.rs`).
   - **`2l1b0.61`**: Verified Pages viewer typed recovery-key entry (`site/index.html`, `site/auth.js`) and non-reused revoked key-slot IDs (`tests/pages_key_cli.rs`).
   - **`gh465-trust-proof-gnr6n`**: Verified release containment retains `ProofDebt` instead of escalating to `Proven` without claim-specific proof (`tests/e2e_search_trust_meta_gate.rs`).
3. **Wave 3 (3 Beads Closed — `bb4d8137`; Receipt `/data/tmp/cass-gate.GspwnK`)**:
   - **`2l1b0.42` (GH #491)**: Verified `--build-hnsw` ANN setup help, `models status --json`, and `capabilities`/`introspect` goldens (`tests/e2e_lexical_fail_open.rs`, `tests/cli_robot.rs`).
   - **`ds7uy.4.1`**: Verified explicit semantic robot-budget two-checkpoint fail-closed execution (`src/lib.rs`, `tests/spec_search_format_contracts.rs`).
   - **`2l1b0.68`**: Verified `_meta.effective` interpretation echo on `search` and `pack`, `_meta.wildcard_fallback_skipped`, and the 9-relation `tests/search_metamorphic.rs` oracle (`tests/search_wildcard_fallback.rs` 63/63).
4. **Wave 4 (23 Beads Closed — `b9416372`; Receipt `/data/tmp/cass-gate.MWVkPx`, `gate: GREEN`, 240 tests + 68 goldens + `STAGE=ubs EXIT=0`)**:
   - **`yb11u`**: `serve --query` one-shot surface aligned in `capabilities.json.golden` and `introspect.json.golden` (`11724d5b`, 68/68 goldens).
   - **`uvii3`**: SQLite source-scan fallback and FTS5 lane `NOT > AND > OR` precedence, local negation, and within-field phrase adjacency (`sqlite_message_scan` 8/8 + `search_metamorphic` 9/9).
   - **`6r17d` (GH #494)**: Resumed candidate with uncounted documents retained to `.lexical-rebuild-quarantine-*` and rebuilt from zero in the same run (`f5a9d48c`, `5f948841`).
   - **`igh4d`**: Primary writer discovers FrankenSQLite virtual FTS (`rootpage = 0`) via `type = 'table'` (`ca621f9d`, `storage_frankensqlite_parity` 39/39).
   - **`9lz4y`**: `doctor_database_quick_check` collects multi-row `PRAGMA quick_check(1)` findings (`bc5f89f5`).
   - **`kz23l` (GH #413)**, **`r5c6h` (GH #440)**, **`cdzcl`**, **`qz43b`**, **`hh4jf`**: Legacy readonly resume FTS shadow preflight, resumed shared-writer checkpoint fingerprint/count finalization, object-only index idempotency cache replay, non-ASCII `source_repo` prefix guard in `parse_bead_refs`, and `usize::MAX` saturating context arithmetic in `view`/`expand`.
   - **`qu81y.6`**, **`qu81y.5`**, **`qu81y.2`**, **`yylyr`**: Zero-parent-env-mutation isolation across `packet_audit.rs`, `tests/util/e2e_log.rs`, `tests/connector_omp.rs` (12/12), and `tests/connector_chatgpt.rs` (82/82).
   - **`i7amy`**, **`gk9ho` (GH #481)**, **`mtt5q` (GH #480)**, **`mlo9d` (GH #476)**, **`931di` (GH #478)**, **`gh466-canonical-lexical-cap-a3fvq` (GH #466)**, **`mduty`**, **`watch-lexical-oom-retry-dh3vd`**, **`6i5ji`**: Bounded `hnsw_present`/`hnsw_state` projection, incremental semantic append preflight before model load, explicit `--embedder` override precedence, marker-independent FTS shadow corpus bound, watch-once symlink connector hint preservation, retained canonical prefix lexical cap, daemon quality model alias/refusal enforcement, connector-qualified watch-once lexical OOM replay debt, and continuous semantic watch cooldown retention/publication.
5. **Wave 5 (24 Beads Closed — `79ef6d57`, `57a3f4b2`, `30ea8e38`, `98c1ea5d`; Receipts `/data/tmp/cass-gate.zT5VH7` and `/data/tmp/cass-gate.EfX6cg`, `gate: GREEN`, 243 lib tests + 10 integration tests + 68 goldens + `STAGE=ubs EXIT=0`)**:
   - **`2l1b0.1`**: Enforced per-filter-word top-level passing test matching in `scripts/gate.sh` (`test_log_counts` + `run_tests` `--filter`), verified by 4 new regression cases in `scripts/tests/dpfvr_ubs_gate_e2e.sh` (`PASS=61 FAIL=0`) and `STAGE=ubs EXIT=0`.
   - **`qu81y` & `qu81y.4`**: Eliminated all remaining `EnvGuard::set` / `CwdGuard` usages across `tests/ssh_sync_integration.rs`, `tests/streaming_index.rs`, `tests/golden_memo_trace.rs`, `tests/search_latency_under_indexing.rs`, `tests/card_defaults_wallclock_ab.rs`, and `tests/util/mod.rs`; enrolled all 35 integration suites in `e2e_subprocess_sources_cannot_mutate_the_parent_environment`.
   - **`l2t7m` & `y7vcg`**: Verified bounded SQLite source-scan post-filter query evaluation (`sqlite_message_scan` 8/8).
   - **`98anf`, `fjp82`, `orz9w`, `apzyi`, `zrtrr`, `3a4ut`, `aocm4`, `4290i`, `wxuu2`, `mt24v`, `h7dla`**: Verified durable-lesson redaction, fail-closed semantic readiness, proof-artifact exit status, redaction diagnostics, structured failure-pattern redaction, short secret detection, segmented provider credentials, secret-scan context scrubbing, key-semantic redaction, strict swarm evidence scrubbing, and fail-closed conversation metadata redaction.
   - **`gyiuc`, `g3s8y`, `uc7nz`, `9md4k` (GH #411), `prxuj`, `0nq2s`**: Verified unified Pi/OMP root ownership, OMP resume aliases and Pi session env truth surfaces (`cli_robot` + 68/68 goldens), OMP HTML export thinking/tool preservation, Oh My Pi v18 identity and legacy remote mirror reclassification, OMP HTML export alias normalization, and Pi-family session-header resume IDs.
   - **`povuc` & `cljkz`**: Verified finite unit fallback on hash-embedder cancellation and FrankenSQLite FTS catch-up / duplicate legacy schema row cleanup (`e2e_search_index`).

---

## Assessment: 2026-09-23 night (DarkFinch)

Scope: local `main` at `71759163` (origin/main `aaa4424a` plus SageSnow's
unpushed dhfzn commit). Read all 1,353 lines of AGENTS.md, all 3,598 lines of
README.md and this plan's 2026-09-21 to 09-23 sections (the
09-04 and 09-01 sections by heading only). Seven read-only audits checked
about 900 README, AGENTS and contract-doc claims (~480 README/AGENTS claims and
~420 contract-doc promises) against the code, each cited to
file:line; the load-bearing findings were re-verified by hand or by running a
binary. Runtime evidence came from:
- a release build of `71759163` (SHA-256 `937cf325448051a6…c97d`);
- a 76-step journey (54 correctness checks) on a synthetic real-format corpus (365 conversations,
  6,617 messages; Claude Code, Codex, Gemini, Aider, Pi-Agent), run on the
  released v0.8.0 binary and on HEAD;
- the owner's live archive, read-only, and two quiescent copies of it;
- a full lib-suite gate.

No live archive was written and no file was deleted.

**Verdict.** On a new or mid-sized archive, cass does what it promises for
agents. Discovery, canonical storage, Quill lexical search with filters,
view/expand/pack, incremental freshness, `serve`, logical `archive`, robot
contracts and diagnostics all work end to end: 50 of 54 checks passed on HEAD,
and the 4 misses are explained below. Four things keep it from delivering the
vision:

1. **It has not worked for its owner for 41 days, but HEAD can fix that.**
   - The owner's live archive holds 2,715 conversations and 3,683,202 messages
     in 16.9 GB, less than half of the owner's actual history. It was last
     indexed on 2026-08-14. The live Quill generation serves 568,527
     documents, the rebuild checkpoint is incomplete, and no scheduler is
     installed.
   - At 21:18 tonight a `lexical_refresh` by an older binary was OOM-killed at
     16.1 GB inside a 16 GB scope. Its governor reported headroom against
     97 GB of *host* memory.
   - On a copy of that archive under the same 16 GB cap, HEAD finished the
     resumed rebuild (2,715/2,715 conversations, 2,222,489 docs) in about
     10 minutes. It then ingested the owner's full history (7,940
     conversations, 8.3M messages; the live archive only ever held 3.68M)
     in about an hour without dying. It was pinned at the cap the whole
     time, ~15 GB of its RSS lies outside what the governor controls, and
     it wrote 120 GB. Its final rebuild needs 82 GB of free disk (`.72`).
   - The canonical file fails the engine's own integrity check: about 1M
     orphaned pages. Every row is still readable, but no repair path exists
     (`.73`).
2. **Documented behaviours give agents wrong answers, not just stale prose.**
   - Date-filtered search fails on any long-lived index (GH #499). The fix is
     unreleased upstream, so the next release would ship the bug.
   - Boolean precedence and parentheses do not work as documented.
   - `cass status --jsn` silently runs `stats`.
   - `forget --apply` leaves the forgotten text searchable.
   - An invalid `--since/--until` is silently ignored.
   - Encrypted HTML exports show the first prompt in plaintext.
   - Exit codes 4/6/8/70 are documented wrongly on the machine-readable
     surfaces.
3. **Nothing reaches users.**
   - 709 commits sit on `main` after v0.8.0, and CHANGELOG describes a v0.9.0
     that was never tagged.
   - The release beads `yrjna` and `2l1b0.25` still have 10 of 13 and 19 of 19
     blockers open (before this pass).
   - HEAD itself is red: clippy fails on a dead function from the unpushed
     dhfzn commit, and the lib suite has 9 failures, two of them untracked
     until now.
4. **The TUI is the surface furthest from its documentation.** Of 126 claims,
   26 are wrong and 24 partial.
   - Ctrl+Shift+C ("copy content") quits.
   - The first run does not index.
   - F12 ranking, F7 context, Ctrl+Space peek and F9 match mode change only a
     label.
   - A blocking y/N update prompt runs before the TUI.

### Evidence gathered in this pass

| Evidence | Result |
|---|---|
| Owner archive, read-only `health`/`status` (installed v0.8.0) | unhealthy/stale; `last_indexed_at` 2026-08-14T02:06Z; 568,527 docs; checkpoint present, not completed; "1818 sessions pending"; no `cass schedule` timers |
| Kernel log and journal | 21:18:41 `cass` pid 2568458 OOM-killed in scope `run-p358335-i921792674` (16G peak, 2.2G swap peak). Lock left at `phase=watch_startup:tantivy_reader_preflight`. Checkpoint committed 507 of 2,715 conversations, pending 897. Runtime block: RSS 16.3 GB, in-flight 1.85 of 2.5 GB, host available 97 GB, controller `steady` |
| Archive shape (read-only copy) | median 246 messages per conversation, p90 2,450, max 307,036 (706 MB of text) in one Codex rollout; the next largest hold 238,975, 125,453 and 111,115 messages; 5.34 GB of content in total |
| C SQLite `quick_check(1000000)` on a quiescent copy | 0 "2nd reference" errors: the 09-03 double references recorded in `scohn` are gone. 32 "free space corruption" lines (known fsqlite dialect). ≥999,968 "never used" pages (the check stopped at its 1M-error limit) in a 4,137,946-page file whose freelist holds 3,826 pages |
| HEAD `doctor check`, full page probe, on a copy (73 s) | `database` fails: "frankensqlite integrity_check: database disk image is malformed: page 8196 is never used (2715 conversations, 3683202 messages)". `archive-db-corrupt`, data-loss risk high, `safe_for_auto_repair:false`, all archive-wide collectors deferred. `next_command` is still `cass doctor --fix`, and `doctor repair --dry-run` produces no plan |
| HEAD `index --background` on a copy, `systemd-run -p MemoryMax=16G` | rebuild resumed from 507 and finished 2,715/2,715 conversations (2,222,489 docs) in ~10 min. Controller `pressure_limited` / `below_emergency_reserve`, in-flight ~0.5 GB, but RSS 15.4–16.5 GB and the scope pinned at its 16 GiB peak. The source scan then ingested the owner's missing history in 61 min, never killed: 7,940 conversations and 8,297,810 messages (Codex alone 6,429 / 6.57M). The live archive had only ever captured 3.68M of them. The canonical DB grew from 16.9 to ~35 GB and `bytes_written` was 120 GB. The run then exited 14 after 71 min: the post-scan authoritative lexical rebuild needs 82.3 GB of free disk and 79.0 GB was available (my scratch copies had consumed it). The message says "index refused to start". The new messages are therefore canonical-only until a rebuild runs (`.72` comment) |
| Read-only searches on the rebuilt copy (`--no-maintenance`, 2.2M docs, host loaded by the concurrent scan) | 7 queries (common term, phrase, `--since`, `--agent`, default hybrid): 2.3–2.5 s wall per one-shot command (5.0 s cold). Of that, `search_ms` is 190–250 ms and `other_ms` ~2.2 s (open and preflight). `--days 7` returned 0 hits without error; this is inconclusive for #499, because the scan's newer sessions were not yet published. Indicative only |
| HEAD logical `archive export` → `verify` → `import` of a copy | export 3,737,204 records (all 3,683,202 messages) into 11.3 GB in 304 s; verify passed in 37 s. Import without flags refuses the v20 archive; with `--allow-compatible-schema`, the migrating import streamed at ~94 MB RSS, slowing from ~230 to ~100 MB/min, and reached 9.4 GB in 59 min. It was then terminated (SIGTERM) during host disk-critical pressure (97% full), which this experiment's own copies mostly caused. **Inconclusive**: there is no integrity verdict on a restored DB (`.73` comment) |
| Synthetic e2e, v0.8.0 → HEAD | 50/54 on HEAD. The misses: `forget` leak (real), encrypted-export plaintext title (real, also in v0.8.0), and two harness artifacts (the missing-index envelope correctly goes to stderr; `--fields summary` omits `created_at`, while the `--days 7` filter is correct with full fields). Full index 61.7 s → 22.6 s; exact-token search p50 wall 180 → 136 ms (engine 80 → 55 ms). Host load was 17–40 and there was no A/A control, so these timings are indicative only, not a performance claim |
| Full lib gate at `71759163` (`/data/tmp/cass-gate.IsJbqy`) | source-identity 0, fmt 0, **clippy 101** (`from_current_process` never used, topology_budget.rs:120), **lib 101: 7,842 passed / 9 failed / 45 ignored** in 148.9 s, ubs 1. Failures: 3 ANN WAL tests (`ds7uy.3.3`), `nohx1`, `962e8`, `fqt9s` watch backlog, and two untracked ones, now `.70` (a global panic-injection race between parallel rebuild tests) and `.71` (gh470 daemon progress) |
| GH #499 root cause | frankensearch `86337594` (2026-09-19) is not an ancestor of `frankensearch-v0.6.1`; the pinned `frankensearch-quill-0.3.1` source has no `live_num_docs` and still raises the error at argus.rs:5009 |
| Red "active" workflows | Codex consumer, OpenClaw native consumer, Raw mirror exclusion, Semantic artifact recovery, Retained semantic WAL and Guarded semantic query CLI have had 0 green runs since 09-21. Both logs read fail to compile: `assert_cmd` missing, `get_connector_factories`/`DiscoveredSourceRole` not found |

### What the audits found (details live in the beads)

| Slice | Claims | Wrong | Partial | Worst findings |
|---|---|---|---|---|
| Storage, sources, ops, env | ~200 | 14 | 18 | "append-only" storage is updated in place; writes run as BEGIN CONCURRENT (fsqlite `concurrent_mode=ON`), not BEGIN IMMEDIATE; SSH uses `StrictHostKeyChecking=yes`; remote install falls through; `CASS_DB_PATH` never read |
| Search, query language, robot API | 154 | 26 | 41 | the ranking section describes removed code; OR binds tighter than AND and parentheses are ignored; trust tiers `trusted`/`failed` are never produced; exit 4/6 mislabelled; `line_number=` maps to `--message-index`; subcommand misroute |
| TUI | 126 | 26 | 24 | Ctrl+Shift+C quits; the kitty keyboard protocol is disabled, so many chords never arrive; first run does not index; blocking update prompt; F7/F9/F12/Ctrl+Space are cosmetic |
| 15 contract docs | ~420 promises | — | — | the swarm contract describes nonexistent flags; the pack contract promises exit 8/10 and `--redaction`; LIMITS, ERROR_CODES and ROBOT_MODE are stale; the Pages recovery key cannot be typed; archive schema migration shipped but is documented as absent |
| Original plans | 7 families | — | — | features exceed the founding plan, but no latency, throughput or memory target is enforced by anything that runs; Pages share profiles (FR-6) were never wired, yet `hkoa` was closed |
| Architecture | — | — | — | rusqlite is dev-only; five production `sqlite3` CLI shell-outs remain, one on a read-only open path; ~10K lines are unreachable; 32 connectors are registered but the docs say 31, 26 or 20 |

### New beads (children of `2l1b0`, label `reality-check-2026-09-23`)

| Bead | P | Gap | Blocks |
|---|---|---|---|
| `.49` | 0 | GH #499: date-filtered search fails after tombstones. Needs a frankensearch release carrying `86337594` and a pin bump; invariant errors must not say `retryable:true` | `yrjna`, `.25` |
| `.50` | 1 | `forget --apply` leaves forgotten text searchable until the next index run | `.25` |
| `.51` | 1 | typo recovery reroutes an exact subcommand (`status --jsn` → `stats`) | `.25` |
| `.52` | 1 | Boolean grammar: implement NOT>AND>OR and parentheses, or rewrite the README | related `.49` |
| `.53` | 1 | TUI ranking modes are inert for non-empty queries | |
| `.54` | 1 | TUI key routing: Ctrl+Shift+C quits, chords cannot be delivered, bulk menu Enter misroutes | |
| `.55` | 2 | TUI F7/Ctrl+Space/F9 are cosmetic; saved views drop the query | |
| `.56` | 1 | TUI first run does not index; blocking pre-TUI update prompt | |
| `.57` | 2 | `CASS_DB_PATH` is advertised but never read | |
| `.58` | 1 | robot contract truth: exit codes 4/6/8/70, kinds, trust tiers, jsonl/`_meta` keys | `.25` |
| `.59` | 2 | docs truth pass for ~60 pure-documentation corrections | related `.26` |
| `.60` | 2 | Pages share profiles never wired; `hkoa` closed because the file existed | |
| `.61` | 2 | Pages typed recovery key missing; revoked key-slot ids are reused | |
| `.62` | 1 | six active workflows do not compile | |
| `.63` | 3 | remove ~10K unreachable lines, only after the owner approves the file list | |
| `.64` | 1 | invalid `--since/--until` silently ignored; date-only `--until` excludes that day | `.25` |
| `.65` | 2 | production `sqlite3` CLI shell-outs, one on a read-only open path | |
| `.66` | 3 | the TUI gives no path to enable semantic search; dead consent dialog | |
| `.67` | 1 | encrypted `export-html` shows the first prompt and session metadata in plaintext | `.25` |
| `.68` | 1 | ambition: no silent substitution. Echo the effective interpretation in `_meta` and add a metamorphic search-semantics oracle | |
| `.69` | 2 | ambition: generate the exit-code, env, connector and dependency tables from the code registries | |
| `.70` | 1 | lib suite nondeterministic: global panic injections leak between parallel tests | `.25` |
| `.71` | 1 | lib test `gh470_daemon_progress…` red: the worker skips message id −1 | `.25` |
| `.72` | 0 | ambition: the indexer lives at the memory cap. Govern resident memory, not only in-flight bytes; chunk giant conversations | |
| `.73` | 1 | ambition: orphaned pages in the owner archive; no repair plan exists, and `next_command` contradicts the check | |

Two findings went to existing owners instead of new beads: `models --list` to
`.42`, and `hnsw_ready` to `wfm4e`. `.58` links to `gnr6n` for trust
semantics.

### Bridge plan (revised in place through three ambition rounds)

The first draft was a list of fixes. The ambition rounds reorganized it around
the failure *classes* and around the one real multi-million-message archive
available locally.

1. **Unblock the owner now (hours).** This is an operator decision; nothing
   was done to the live archive.
   - Stop old binaries from auto-refreshing the live archive: they die at the
     cap. Use `CASS_AUTO_REFRESH=0` until a build containing 4e1248e9/dhfzn is
     installed.
   - Back up the canonical DB.
   - Rebuild it losslessly through `archive export` → `import`
     (export and verify are proven on the owner's data; import is unproven at this scale, see `.73`). Then run a foreground HEAD `cass index` under the
     owner's memory cap. On the copy, the rebuild took ~10 min at 16 GB.
   - Then `cass schedule install`. Without the scheduler the archive goes
     stale again.
2. **Make HEAD green (a day).**
   - dhfzn dead code (owner notified).
   - `.70` panic-injection race and `.71` gh470 test.
   - Keep `ds7uy.3.3`, `nohx1`, `962e8` and `fqt9s` with their owners.
   - `.62` so the narrow workflows give signal again.
3. **Close the wrong-answer and privacy class before release (days).**
   - Instances: `.49` (pin bump; a 0.6.1-based hotfix release is the
     low-risk path), `.50`, `.51`, `.64`, `.67` and `.58`, all wired as
     release blockers.
   - Class fix: `.68` makes the class checkable (effective-interpretation
     echo plus a metamorphic oracle), so a new flag cannot repeat it silently.
4. **Ship v0.9.0.** Include the blocker batch above and an honest capability
   matrix (`.25`). Decide the strict-UBS path under `5v57k` without weakening
   the gate. Rewrite the CHANGELOG entry as Unreleased until the tag exists.
5. **Govern memory for real (`.72`, P0 mechanism under `.44`/`.46`).**
   - Attribute the ~15 GB of ungoverned RSS: heap profile, allocator A/B with
     an A/A control.
   - Control resident memory with feedback (setpoint below `memory.max`, AIMD
     on workers/chunk/commit cadence).
   - Chunk giant conversations by message range.
   - Checkpoint before crossing the setpoint.
   - Acceptance: the owner-archive copy under 16 GB with peak ≤85% of the cap,
     and a generated skew fixture (one conversation ≥300k messages) under a
     4 GB cap.
6. **Archive integrity as a product feature (`.73`, with `scohn`).**
   - Find the engine page leak upstream.
   - Add a fingerprinted, lossless reconstruct plan to `doctor repair`.
   - Make `next_command` agree with the check that failed.
7. **Truth by construction.**
   - `.69` generates the tables; `.54` generates the keymap docs; `.59`
     corrects the rest.
   - Then the TUI behavior beads `.53`/`.55`/`.56`/`.66` and Pages
     `.60`/`.61`.
   - `.63` dead code, with the owner's permission.

Ambition record:
- **Round 1.** The ~60 wrong claims reduce to four classes:
  - silent substitution (`.68`);
  - hand-maintained duplicate tables (`.69`);
  - closure without a user-surface probe (`hkoa`, "fixed on main" closures);
  - scale untested on the one real fixture.
- **Round 2.** The owner archive became the acceptance fixture. Its shape
  (extreme skew) and the governed-vs-resident gap produced `.72`; the engine
  integrity verdict produced `.73`.
- **Round 3.** Chose the mechanisms:
  - AIMD feedback on resident memory, with the in-flight bound as an inner
    limit;
  - bin-packing by predicted per-message cost with message-range chunking;
  - metamorphic relations (OR monotone up, AND down, window nesting, deletion
    locality) as oracles for the wrong-answer class;
  - lossless logical reconstruction as the repair primitive.

Refinement record (six passes over the new beads):
1. **Test and log standard.** Every new bead received two acceptance criteria:
   real-binary tests with E2E_LOGGING_SCHEMA logs plus a negative case that
   fails on `71759163`, and a gate receipt plus strict UBS.
2. **Ownership.** `models --list` moved to `.42` and `hnsw_ready` to `wfm4e`;
   `.58` links `gnr6n`. Pages, export, TUI-semantic and first-run beads link
   their proof owners (`.23`, `sd5o9`, `ds7uy.5`, `.22`).
3. **Risk.** `.49` records the 219-commit upstream delta and the hotfix
   option. `.54` isolates the Ctrl+C arm fix from the kitty-protocol work and
   requires an escape hatch.
4. **Evidence.** `.50` now records that incremental and full re-index do not
   resurrect a forgotten session, so the defect is a leak *window*. Known
   failures got receipt comments (`fqt9s`, `nohx1`, `ds7uy.3.3`, `962e8`); the
   two untracked failures became `.70`/`.71`.
5. **Consistency.** Release certification (`.25`) needs a green full lib
   suite, so `.70` and `.71` became blockers of `.25`. A sixth pass found
   nothing further. `br dep cycles` is empty and
   `bv --robot-triage` ranks `.49` and `.50` as the top two picks.

This is not proof that every gap was found. Historical plans were read by the
audits, not re-read by me line by line.

### Answers to the five questions

1. **What works.**
   - Everything the synthetic journey exercises: discovery for 32 connectors,
     canonical storage, lexical search with filters, pagination and
     aggregation, view/expand/pack with exact identity, incremental
     freshness, robot contracts and envelopes, doctor/triage/health,
     `serve --stdio`, logical `archive` export/verify, and bookmarks.
   - On the owner's scale, HEAD rebuilds the lexical index under a 16 GB cap.
2. **What does not work.**
   - The owner's live archive: stale for 41 days, with an integrity failure.
   - Date filters on long-lived indexes (#499).
   - Boolean grammar, `forget` purge, encrypted-export privacy, time-filter
     validation, command typo routing and exit-code documentation.
   - A third of the TUI's documented controls.
   - A green HEAD: clippy, 9 lib failures and 6 red narrow workflows.
   - A release.
3. **What blocks.**
   - An upstream frankensearch release (#499).
   - Resident memory with no headroom at scale.
   - The owner's operator decision on the live archive.
   - Release qualification, including strict UBS.
   - 93 of 110 in_progress beads idle for more than 7 days hide which work is
     live, and bv skips its priority-mismatch and duplicate checks above
     2,000 issues (the tracker holds 2,294).
4. **Would the open beads close the gap?** Not before this pass. Twenty-five
   gaps had no bead: 19 found by the audits and the e2e run, 2 lib
   failures and 4 from the ambition rounds. With them, the graph covers every gap found tonight.
   Closure still requires shipping a release and an operator action on the
   live archive, and neither is a bead that closes itself.
5. **Vision goals with no bead before this pass.**
   - #499 (filed 01:50 UTC, within the 24-hour window).
   - The `forget` leak and the encrypted-export plaintext.
   - Typo misroute, Boolean grammar, time-filter silence, `CASS_DB_PATH`.
   - TUI ranking, keys, cosmetic toggles, first run and update prompt.
   - Pages profiles and recovery key.
   - Red workflows, dead code, `sqlite3` shell-outs.
   - Resident-memory governance and orphaned-page repair.

## Update: 2026-09-23 (SageSnow)

**The first complete lib-suite run in weeks** (`0e77ef78`, 12,600 s cap,
receipt `/data/tmp/cass-gate.jkf85b`): 7,758 passed, **82 failed**, 44
ignored. Earlier gates never finished the suite inside the 2,400 s cap, so
nothing showed how red `main` actually was. The failures break down as:

| Cause | Tests | Resolution |
|---|---|---|
| `cf0fdb85` prettier-formatted six pinned third-party Pages vendor assets; their SHA-256/size pins failed | ~60 (`pages::*`) | Reverted in `aa47c7f4`; all six match their pins again, pins unchanged (`ggq7g`) |
| `36c5d4d5` (#450, mine) added an error kind without bumping the audited count | 1 | `8325ddfd` |
| TUI cached-detail/find-bar/footer, probably since `d499554f` (#493 anchors) | 7 | New bead `2gy3y` |
| Semantic FSVI v2 / native-ANN WAL fixtures | 5 | Existing owners `962e8`, `ds7uy.3.3`; exact-search case `nohx1` |
| Indexer: watch backlog overflow (new test), rebuild skip-when-complete metadata, gh473 budget | 3 | New bead `fqt9s` |
| Watch lexical OOM retry | 1 | Existing `dh3vd` |
| `gh477` read-only opener | 1 | Passed on re-run; load-sensitive (`vpsls`) |

At `da06837c` the same filter set is down to the 16 tracked residuals, with
all pages and error-kind tests green (receipt `/data/tmp/cass-gate.eqOyiv`).

**GH #498 fixed (`da06837c`, `o5d74`).** A full rebuild of a >4.2M-message
archive could never commit:
- Quill's in-commit tier merge folds any eight same-tier segments, and its top tier has no bound.
- After cass's byte-capped end-of-rebuild fold left eight or more large segments, the next commit folded them past the per-term posting limit (2^22).

cass now disables the engine tier merge (`tier_fanout = usize::MAX`) and bounds
every planned fold at `MAX_FOLD_OUTPUT_DOCS`. A real-engine reproduction covers
the reporter's path: 4.24M documents in eight large segments.
- The engine default fails with the reporter's exact error.
- The cass config commits and folds into 3.71M + 0.53M segments.

The upstream planner needs the same bound (noted on the bead).

**Gate defect fixed (`01854fa3`).** The pinned UBS runner scans every
explicitly named file even when `.ubsignore` covers it. So the config-level
suppression AGENTS.md prescribes never applied in the gate's changed-file
mode. The gate now filters by `.ubsignore` and prints each skipped path.
On this change set that skips exactly the five scanner-visible vendor
files; the three owned files stay scanned.

**Other outcomes:**
- #467: closed, verified by its reporter on darwin-arm64.
- #369 bead `cb0gl`: closed on its probe.
- #483: the conv-192 stall no longer reproduces on `main` (reporter's retest); what blocked them was #498.

**Later on 2026-09-23:**
- **TUI residuals fixed** (`7aa102c8`, `2gy3y`). Seven `ui::app` tests had relied on the same-path fallback that #493 removed. Their fixtures now build a canonical cached detail for the selected hit; `ui::app::tests` passes 1024/1024 and no assertion changed.
- **Rebuild fast-path test fixed** (`6f86acc8`, `fqt9s` item 2). It now proves the completed-checkpoint fast path over a real generation instead of planting a checkpoint that #494 correctly refuses.
- **Lock breadcrumb fixed** (`24366433`, `7qirz`). `index-run.lock` now names `index:scan` and `lexical:rebuild`. The #483/#497 reporter had seen a finished preflight step named for a whole 42-minute pass.
- **#496 headroom fixed** (`149ee210`, `ztlqc`). The full-rebuild headroom preflight doubled a failed rebuild's leftover `.rebuild-staging` generation, locking a 13.4 GB archive out of its retry. The requirement is now documented, and it can be queried through `cass doctor --json` (`storage_pressure.full_rebuild_readiness`).
- **Diagnosed, not fixed:**
  - `nohx1`: frankensearch-index 0.3.1 `append_batch` tombstones superseded main rows, so the fixture's byte-equality check is stale.
  - `2l1b0.46`: a `--full` run's own ingest invalidates its checkpoint fingerprint; this needs a prefix-based resume proof.
  - `kfem7`: the gh439 slow-repair test stalls before its first breadcrumb on a pressured worker. It fails 3/3 after `89e053f3` and passed once before, but that commit's code cannot reach the path. The lock snapshot points to acquisition blocked in `sync_all()`.
- **Duplicate avoided late.** My #497 retirement fix lost a race with `89e053f3`. I dropped my unpushed commit and closed its bead as superseded.

## Assessment: 2026-09-22 evening (execution pass, SageSnow)

This section supersedes current-state statements below. The midday assessment
that follows remains the record of the audit; this pass executed against it and
re-measured. Source baseline `407fdbc0`. The first thirteen commits (`721fa438..0c2cab13`) reached `origin/main` through another agent's merge `4b611d7c`; the rest were rebased onto it.

**Verdict:** the core product works end to end on current source. A clean
synthetic run on a HEAD binary (five real-format providers plus a generated
Claude corpus, 406 conversations / 8,019 messages) indexed in 13.7 s, answered
one-shot lexical searches in 39–45 ms wall (18 ms engine), filtered exactly by
workspace and agent, viewed/expanded/packed hits, picked up an appended message
after a 2.5 s incremental run, reported hybrid-without-model truthfully as a
lexical fallback, and doctor called it healthy. What users are missing is not
features but **shipping**: 626 commits sit on `main` after v0.8.0 (a
`release(v0.9.0)` commit `4391672a` exists, untagged), and about two thirds of
the open GitHub issues are fixed on `main` but unreleased. The remaining red is
concentrated in a few places: large-archive memory (GH #320), incremental ANN
(GH #460), the native-ANN WAL admission tests owned by `ds7uy.3.3`, the strict
UBS gate, and e2e tests the #493 identity work left behind
(`2l1b0.43`). The search-timeout contract conflict that the day's branch merges
brought onto `main` is resolved (`vy4ic`: exit 10 for explicit semantic).

### What was verified directly (not inferred)

1. **Gate harness defect, fixed (`721fa438`).** `scripts/gate.sh` hashed the
   tracked `tests/.beads/*` fixtures, which rch's global `.beads/` transfer
   exclude never sends, so source identity passed on a worker with stale
   leftovers (hz2) and failed on a clean one (hz3) for the same tree, before any
   stage ran. No build or test reads those files.
2. **`main` did not pass `cargo fmt --check`.** 62 files landed unformatted
   (also the red stage of three push workflows). Fixed style-only with the
   pinned rustfmt (`a766a833`); the fmt stage is green since.
3. **A regression from today's #495 landing, reproduced on real binaries.** On
   an archive whose shadow uses the legacy 7-column DDL, the HEAD binary's
   `cass sources agents exclude codex --robot-format json` exited 5
   (`archive-fts-rebuild`) after already purging rows; released v0.8.0 exits 0
   and leaves the canonical `content=''` registration. Cause: residue
   classification made `rebuild_fts_via_frankensqlite` bail for every direct
   caller (exclude purge, reset, forget, dedup). Fixed in `530e3124`.
4. **Released-binary acceptance for two long-held issues.** Using the published
   v0.7.1 and v0.8.0 tarballs (checksums verified; the installed `cass` is
   byte-identical to the v0.8.0 asset): GH #459's repair journey (v0.7.1 writes
   the slug-decoded workspace, v0.8.0 `index --full` repairs it exactly; lexical
   and hash-semantic `--workspace` both correct; counts conserved; idempotent;
   transcript untouched) and GH #449's local Devin store (main chain indexed
   with its working directory; abandoned branch and hidden session excluded;
   source DB unchanged). Both issues are closed with that evidence.
5. **Triage of all 50 open issues** by five read-only investigators, each claim
   checked against HEAD code, `git merge-base --is-ancestor <sha> v0.8.0`, and
   the pinned registry sources. Several maintainer comments were stale or wrong
   (GH #489's cap is cass's own, not FAD's; GH #483's "#413 fix not in 0.8.0"
   is false; GH #426/#496 "scan resume still open" is stale; GH #467's pin note
   predates the adopted fixed frankensearch; GH #470/#471/#472 "no
   implementation" predate their commits).
6. **The #495 fix, on a release build of this pass (`6dd37b8d`).** The same
   legacy-DDL archive now purges 2 conversations / 6 messages for
   `sources agents exclude codex`, rebuilds lexical, and exits 0.
7. **`sources agents exclude` misreported an unreadable archive** (same build):
   an 8 KB non-SQLite `agent_search.db` gave exit 0 and "No already archived
   data for that agent was present", with the file untouched. Fixed as `dndyv`.
8. **`cli_robot` is not hermetic on a fresh checkout** (`uxz8y`, a regression of
   the closed `xwi3f`): ~51 tests read `tests/fixtures/search_demo_data` in
   place, the fixture has no `v9-quill` index, so the first searches rebuild it
   inside the checkout and parallel tests hit `index-busy` (8 failures on a
   fresh worker path). v0.8.0 rebuilds the fixture the same way; warm workers
   hide it.

### Gate receipts (`scripts/gate.sh`, rch, pinned worktrees)

| HEAD | Receipt | Green | Red, and why |
|---|---|---|---|
| `0c2cab13` | `/data/tmp/cass-gate.ix6Y8E` | fmt, clippy, `cli_logical_archive`, `spec_search_format_contracts`, both connector-completeness binaries, goldens (regen + verify) | Codebuff e2e (test bug, fixed `81b04511`); `cli_robot` 8 `index-busy` + 1 codebuff golden (fixed `206acf7a`, `81b04511`); UBS |
| `dc950c40` (full) | `/data/tmp/cass-gate.UespkV` | clippy; `cli_logical_archive` 18, Codebuff e2e, connector completeness, `cli_robot` 313, `e2e_sources` 117, `e2e_semantic_backfill_robot` 15, `e2e_jsonl_schema_test`, goldens | fmt (merge whitespace, fixed `4bc2d40e`); lib hit the 2,400 s cap at 2,134 of 7,871 tests on a contended worker (3 failures: 2 stale tests fixed in `0a1be788`, 1 FSVI v2 tombstone test handed to `962e8`); `spec_search_format_contracts` (contract conflict `vy4ic`, plus a no-match run that passes on a release build); `cli_schedule` (120 s test cap under load; a release build finishes in ~3 s); `e2e_cli_flows` view/expand and `e2e_lexical_fail_open` pack ID (tests predate the #493 identity changes, owner `2l1b0.43`); UBS |
| `4bc2d40e` (fresh path) | `/data/tmp/cass-gate.7zL9ky` | fmt, clippy, `cli_robot` 314, goldens | `e2e_lexical_fail_open` pack ID (as above); UBS |
| `0a1be788` (touched modules) | `/data/tmp/cass-gate.CW5VG9` | fmt, clippy, lib 560 passed / 0 failed | UBS |

Two process findings: the full lib suite no longer fits the gate's 2,400 s lib
cap at `-j6` on a shared worker, so full-suite receipts need a longer cap or a
quieter worker; and today's merge of July/August branches (`4b611d7c` line)
brought in a second budget contract (`vy4ic`) and left five e2e tests
disagreeing with code that had since changed.

### Issue status after this pass (50 open at start)

| Class | Issues |
|---|---|
| Closed with evidence | #459, #449 (released-binary acceptance), #413, #390, #329, #395 (fixed in shipped releases, reporters invited to reopen), #489 (validated by the reporter on `main`), #422 (fixed in v0.8.0; `692998f1` adds the timeout stderr note), #475 (audit delivered) |
| Fixed on `main`, unreleased (close at release) | #462, #477, #476, #379, #494, #473, #474, #426, #466, #463, #480, #469, #471, #472, #478, #468, #465, #464, #447, #415, #388, #423 |
| Partial: this pass closed the named gap, other acceptance remains | #495 (direct-caller regression), #497 (legacy `content=''` residue, `032464af`), #374 (rowid shadow residue loop), #438 (explicit repair now rewrites old segments), #381 (Windows self-updater), #461 (per-run full content scan), #496/#483 (cgroup-blind budgets) |
| Still open on cass | #320 (indexing peak memory; profile first), #460 (incremental HNSW append, L), #470 (passage gaps above 4,080 chars), #452 (semantic serving in `cass serve`), #349 (in-place v14 migration), #382 (ARM64 MiniLM needs a native run on the adopted pin) |
| Upstream | #391 (frankensqlite#397) |
| Decided (owner delegated), then acted on | #481 (no in-process GPU; an opt-in external-endpoint embedding space with a known-answer probe is the direction, `gl29d`), #450 (policy implemented in `36c5d4d5`; open until release), #475 (skill-recipe audit done and filed in the skills repo, issue closed) |
| Reporter info | #443 (retest on v0.8.0; follow-up posted), #467 (needs one darwin-arm64 run) |

### Work landed in this pass

| Commit | Change | Bead |
|---|---|---|
| `721fa438` | gate identity excludes rch-untransferred `.beads` fixtures | `2l1b0.1` |
| `a766a833` | rustfmt 62 unformatted files (style only) | — |
| `612bd808` | Windows self-update passes URLs to the script block via `-EncodedCommand` (every Windows `cass upgrade` failed) | `6gmrz` (new) |
| `530e3124` | direct `rebuild_fts` callers converge residue; preflight stops re-retiring an already retired shadow each run; colon-separated `CASS_EXCLUDE_PATHS` warns | `2l1b0.45`, `2l1b0.40` |
| `692998f1` | timed-out robot search says so on stderr | `u3vho` |
| `8139d95a` | expected lexical doc count memoized with an append-only delta (was a full content scan, twice per incremental run) | `3zq55` |
| `4e1248e9` | memory budgets clamp to the cgroup (v2/v1), including the Tantivy writer heap | `2l1b0.46` |
| `818d3c0b` | explicit `--watch-once` names existing paths no connector claims | `931di` |
| `b28acf31` | explicit FTS repair of a parity-healthy shadow runs FTS5 optimize + integrity-check | `2l1b0.3` |
| `6fe5524e` | rowid-shadow registrations classified as residue (the `content=''`-only family landed concurrently as `032464af`) | `2l1b0.47`, `zp0fp` |
| `87f3886f` | README/AGENTS: schema 21, toolchain date, workflow state, `serve`/`archive` | `2l1b0.26` |
| `4a427f3d` | backfill receipts count real model constructions | `ofnlv` |
| `d21aece3` | Codebuff / Freebuff connector wired (FAD 0.3.0 `codebuff`) | `2l1b0.12` |
| `7ce02589`, `1dadad6b` | compile and clippy fixes for the above, caught by the gate | `2l1b0.45`, `ofnlv` |
| `0c2cab13` | `cass archive` failures classified by error type: usage 2, integrity 5, busy 7, I/O 14 (was: every failure exit 2) | `ukg62` (new) |
| `0c298788` | PowerShell real-shell test names each interpreter by a literal (two UBS criticals it had introduced) | `6gmrz` |
| `819d985e` | `sources agents exclude` no longer says "no archived data" when the archive is missing or cannot be opened (reproduced on the release binary) | `dndyv` (new) |
| `72572068` | archive import/search/view report corruption as integrity (5); a failed read mid-archive stays I/O (14) instead of looking like corruption | `gdwzy` (new) |
| `81b04511` | Codebuff e2e test stamps its edit after the previous run (the backdated edit was correctly skipped by incremental discovery; verified on the release binary) and gains a stale-text negative; codebuff added to three robot goldens | `2l1b0.12` |
| `206acf7a` | `cli_robot` observers read one pre-warmed private copy of the demo fixture; a guard test forbids pointing commands at the checkout | `uxz8y` (new) |
| `4bc2d40e` | rustfmt of a merge-introduced blank line (fmt was red on `main`) | — |
| `0a1be788` | two lib tests whose premises other changes made stale (legacy-DDL "healthy" fixture; Codex budget message) | `2l1b0.45`, `2l1b0.40` |
| `00a7403a` | a spec test's failure message shows the observed budget object | `u3vho` |
| `c3ac835a` | decision: explicit semantic budget exhaustion is a typed exit-10 timeout (ds7uy.4.1); spec test and README/AGENTS aligned | `vy4ic` |
| `36c5d4d5` | decision for #450: `--background` runs never start the one-time storage migration repair on a large unmigrated archive (exit 7 `migration-repair-pending`); foreground `index` still performs it | `2l1b0.6` |
| `d34142f5`, `ec7fd8b1`, `557cf59b` | real-binary #495 acceptance: orphan shadow tables, legacy DDL and legacy DDL missing a `_docsize` row converge through both `doctor --rebuild-canonical-fts --yes` and `index --full`; canonical rows unchanged | `2l1b0.45` |
| `5ab5f7d9`, `640d2c6f` | #486: no byte of the data dir contains an excluded sentinel (with a positive control) | `2l1b0.37` |

Beads closed on cited evidence: `sxhgy`, `2l1b0.14`, `eayhf`, `ukg62`,
`gdwzy`, `dndyv`, `ljf90`, `0dbkt`, `ubq10`, `test-env-api-isolation-qu81y.1`.
New beads fixed in this pass: `6gmrz` (Windows updater; a native Windows run
remains), `ukg62` (`cass archive` mapped every failure to exit 2), `gdwzy`
(archive readers other than `verify`), `dndyv` (misleading exclude purge
message), `uxz8y` (fixed; its runtime fixture-hash check remains open). New
open beads: `5v57k` (strict UBS qualification; now blocks the release bead
`yrjna`) and `vy4ic` (explicit semantic budget exhaustion: exit 10 per
`ds7uy.4.1` vs exit 0 per README and the spec test).

### The strict UBS gate is now the main thing holding bug beads open

A read-only audit of 58 stale `in_progress` bug beads (idle 1–8 weeks) found
about 30 whose fix and named regression tests are present on `main`. Most of
them record the same closure condition in their owners' notes: combined RCH
fmt/clippy/tests **and strict UBS**. Strict UBS is red across the repository
for pre-existing findings (a local `ubs --ci` over just eight files touched
here reports 13 critical and 1,310 warnings, mostly `unwrap` in tests and
test-only `panic!`). Those beads were deliberately left open: closing them on
test evidence alone would override their stated gate. The honest next step is
a UBS qualification pass (triage by rule, fix real findings, record reviewed
suppressions per the documented policy), not per-bead closure (`5v57k`, now
blocking the release bead `yrjna`). Four beads whose stated gate was only the
RCH runtime receipt (`ljf90`, `0dbkt`, `ubq10`, `test-env-api-isolation-qu81y.1`)
are closed with the receipts above. Audit verdicts that did not survive a
re-read of the bead: `wjpv5` (document-count gating is not the equivalence
proof its analysis requires), `kz23l` (requires strict-UBS receipts),
`hh4jf` (its view/expand probe now fails against the #493 output), `962e8`,
`cooow`, `jfkqh`, `fyepq` (real gaps), `lgpqg`, `uc7nz`, `kupq4` (no
implementing commit).

The follow-up triage (recorded on `5v57k`) measured where the findings are.
Over `src/` (368 files) the pinned-family scanner reports 306 critical,
57,545 warning and 21,374 info. Excluding test code removes about 94% of the
warnings (the direct Rust scanner without the ast-grep pack: 30,622 warnings
drop to 1,782 and 93 criticals to 55). The 55 production criticals reviewed
here are all false positives: redaction regex constants and table or env-var
names read as "secrets", enum comparisons read as "timing-unsafe", and cass
re-launching its own binary. Two tooling facts decide the path. The pinned
UBS v5.4.4 already supports fingerprinted `--save-baseline` / `--baseline
--new-only`: replaying `612bd808` against its parent reports exactly the two
`Command::new(shell)` criticals that commit introduced. But the same replay
shows 14 new *test* warnings, and the Rust module's `--exclude-tests` is not
exposed by the `ubs` runner. The proposed gate is therefore delta +
exclude-tests + fail-on-warning: new production warnings still block, and it
needs a UBS runner change and release first. Dropping `--fail-on-warning`
without it would be a gate weakening, so the gate is unchanged.

### Answers to the five reality-check questions

1. **What works:** discovery for 32 connectors, canonical FrankenSQLite
   storage, Quill lexical search with filters and pagination, view/expand/pack
   with exact identities, incremental indexing, robot contracts and error
   envelopes, doctor/triage/health surfaces, released-binary repair journeys,
   the persistent `serve` lexical service and the logical `archive` commands.
2. **Not working or unproven:** large-archive memory at 1M+ messages (#320,
   #496 now cgroup-bounded but not profiled), native ANN over retained WAL
   updates (`ds7uy.3.3`: a WAL replacement tombstones main-file rows, and
   frankensearch's `FsHnswIndex::try_load_native` then refuses the unchanged
   graph; the fix is a tombstone-tolerant graph binding upstream, then a pin
   bump), incremental HNSW (#460), progressive two-tier
   (inactive by design), ARM64 native MiniLM on the fixed pin (unrun), strict
   UBS, and anything that requires a release to reach users.
3. **What blocks:** a release, and in front of it the strict UBS gate. The
   code for most user reports exists; the owner decision to cut v0.9.0 (with
   the release gates in `yrjna`/`2l1b0.25`: full lib + integration + goldens
   green, strict UBS qualification, per-target builds, a darwin-arm64 semantic
   run, a native Windows upgrade run) is the step that turns it into user
   value. UBS qualification is also the step that lets the code-complete bug
   beads close honestly (see above).
4. **Would the open beads close the gap?** Mostly: every open issue now maps to
   a bead, and the remaining cass-side gaps are tracked (`2l1b0.44/.46`, `of9i7`,
   `2l1b0.33`, `2l1b0.9`, `qpdtd`, `dl6ye`, `ds7uy.3.3`). Not by themselves: 113
   beads sit `in_progress` under agents idle for 1–8 weeks, which hides which
   work is live, and release acceptance is scattered across several epics.
5. **Vision goals with no bead before this pass:** the Windows self-updater
   failure (`6gmrz`, fixed), `cass archive` exit-code conflation (`ukg62`,
   fixed), the misleading exclude purge message (`dndyv`, fixed), `cli_robot`
   hermeticity (`uxz8y`), the conflicting search-timeout contracts (`vy4ic`),
   strict UBS qualification as a tracked task (`5v57k`), the gate identity
   defect (fixed), and the fmt-red `main` (fixed twice).

## Midday assessment: 2026-09-22

This section supersedes current-state statements below. Earlier sections retain
their dated evidence; neither their completion labels nor their measurements
transfer to today's source or release.

**Verdict:** CASS has a substantial working implementation and useful targeted
test evidence. Dependable, bounded retrieval from large archives and a fully
qualified release remain unproven. The best next work is to finish publication,
repair and retrieval paths already present, reproduce the remaining failures,
and deliver their fixes in exact tested artifacts. More feature breadth is not
the limiting factor.

### Scope, provenance and completed audit TODO

Baseline: clean `main`, `c1bd4d30`. Read **all 1,329 lines of AGENTS.md and all
3,574 lines of README.md**, then the complete 12-line concurrent addition
(3,586 lines at closeout). Reviewed the current bridge assessment, historical
sections, installer and sync decisions, search-service contract, selected plans,
production seams and test obligations. Historical planning material was
inventoried and selectively reviewed, not exhaustively reread; this is not an
every-line source or security audit.

- [x] Coordinate as RainyOrchid through Agent Mail; reserve this existing plan.
- [x] Read governing documents and distinguish current contracts from history.
- [x] Inspect Git, current code, tracker, public issues, release and workflows.
- [x] Attempt bounded, nonmutating installed CASS history retrieval.
- [x] Compare vision with implementation and actual retained test evidence.
- [x] Revise the bridge, add only uncovered issue-specific acceptance, preserve owners.
- [x] Apply three ambition passes and five refinement passes, then validate graph.
- [x] Report failures and missing evidence without claiming implementation closure.

Artifact creation check: the consumer is the user who explicitly requested this
end-to-end reality check; it gates this assessment and the proposed work order.
Observed defects are red validation, absent issue acceptance and stale capability
claims. This revision retires as current guidance when superseded by a later
evidence-based assessment. It earns **no new runtime capability credit**. No
integrity-control exception is needed. Publication/retrieval repair is the
higher-value implementation work after this explicitly requested audit; do not
continue generating reports instead of executing those tasks.

### Evidence that changes the assessment

1. **Shipped and source are different.** The live GitHub API still identifies
   [v0.8.0](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.8.0),
   published September 10, target `96510ff5d06f678d25d5c786d058e4b6ea628827`.
   Installed CASS reports 0.8.0; Cargo.toml says 0.9.0. Exact dependency pins are
   already SQLite 0.4.4, Asupersync 0.5.0, FrankenSearch 0.6.1 and FAD 0.3.0.
   Do not repeat the upgrade or infer consumer safety from publication.
2. **The latest selected remote gate is red.** Job `30023605353973918`, worker
   `hz2`, retained source-content SHA256
   `83e430ac90cc02dd8f139937f89c3d6ba8d3b424e5531ae7b0f3c03a19d2e691`:
   source identity/freshness passed; fmt exited 1; Clippy exited 101 at
   `src/logical_archive/migrate.rs:907`; library tests passed 68 and failed 3;
   prune CLI passed 6; goldens passed 68. That is **142 selected passing tests,
   three failures**, not the full suite. UBS exited 1 with two critical labels
   and 1,773 warnings across six whole Rust files, not all triaged. Counts are
   scanner classifications, not proven defects or a comparable trend.
   Receipts: `/tmp/cass-rainyorchid-reconciled-gate.log` and
   `/tmp/cass-rainyorchid-reconciled-console.log`. These are retained session
   artifacts, not permanent public release evidence.
3. **Native ANN has three real failing tests.** Latest WAL replacement,
   unmodified chunks of a replaced message, and source-local shadowing all
   stop at `SidecarNotNative / selected_graph_not_native`. Fixture byte and
   tombstone invariants now pass. The tests do not reach returned-result
   comparison; this establishes failed native admission, not wrong search
   results. Keep exact fallback and identity checks intact. `ds7uy.3.3` owns
   the remaining contract, including all authoritative shards.
4. **GH494 received no runtime test coverage in that gate.** The requested
   `indexer::gh494_tests` filter was wrong; the file is included inside
   `indexer::tests`. Correct selector: `indexer::tests::gh494`. A positive union
   count concealed an empty individual selector. Gate task `.1` now explicitly
   requires nonempty discovery/coverage for every requested selector.
5. **Useful fixes have passed narrow tests.** Raw-mirror preview/audit behavior,
   bounded actual manifest reads under file growth, and corresponding CLI
   controls have real selected evidence: 23 mirror unit tests and six prune
   CLI tests in the latest gate. The manifest cap is not an inventory-wide
   RSS cap; preview locking is not read-only-filesystem compatibility; audit
   syncing does not eliminate the unlink-to-result crash window. `.39` stays open.
6. **The audit's live history query returned no evidence.** Installed 0.8.0,
   lexical mode, project scope, `--no-maintenance`, two-second query budget and
   ten-second external deadline returned code 5, `maintenance-required`, with
   an incomplete lexical rebuild checkpoint. No archive repair was attempted.
   This is a truthful refusal on this archive, not useful retrieval or proof
   that all CASS installations fail.
7. **Workflow state is mixed.** General CI, Browser Tests and Release are still
   manually disabled; many focused repository workflows are active. The README
   statement that every workflow is disabled is false. Enabled status alone
   establishes no passing job or exact-source acceptance.

The gate preceded this documentation-only assessment; no new Rust build/test
was run for the audit. The post-run debug executable hash is not in-test ELF
attestation. No performance claim follows from these tests.

**Late merge and independent CI observation:** `03ede16a` arrived during the
audit. Compared with the baseline it changes README, Beads and the FTS workflow,
but no Rust source. The added README table documents existing lexical rebuild
controls and checkpoints, partially addressing `.44/.46` discovery work.
GH495's repair exists as an embedded patch applied inside the workflow, not in
the checked-out Rust files. Its latest
[run 35745828016](https://github.com/Dicklesworthstone/coding_agent_session_search/actions/runs/35745828016)
at event SHA `609fc994` failed: **106 passed, one failed, one ignored**. The new
residue tests pass, but the healthy-FTS skip test expects `AlreadyHealthy { rows:
4 }` and receives `Rebuilt { inserted_rows: 4 }`. Investigate that behavior and
reuse the patch; neither recreate it nor change the assertion without diagnosis.

The same workflow would push four patched shared Rust files directly to main
after only filtered library tests. It has no Clippy, strict UBS or CLI integration
stage, and checks out mutable `main` before patching. That is a concrete gap in
the mandatory publication gate, now recorded in `.1`. The failed run did not
reach publication. No workflow was changed or triggered by this audit; patched
CI source and event SHA are not automatically the same artifact identity.

### Current vision coverage and architecture

Provider discovery/normalization and raw capture feed canonical FrankenSQLite
storage. Quill, vector/ANN generations and analytics are derived. Search must
preserve identity through filtering, hydration and follow-up; packs additionally
budget and redact evidence. Publication, reader ownership, migration and repair
are part of this user journey, not incidental setup costs.

| Promise | Current assessment | Evidence and remaining work |
|---|---|---|
| Cross-provider local ingestion | Implemented, qualification incomplete | FAD plus CASS connector seams; `.35/.37/.38/.40`, real released-consumer and privacy cases |
| Preserve canonical history | Implemented, critical acceptance unresolved | Storage and recovery exist; `.2/.3/.45/.47` distinguish real corruption, schema disagreement and derived residue |
| Publish usable rebuilt lexical assets | Implemented, reported failure unresolved | GH494 tests/code exist; `.48` requires new live authority and fresh searchable evidence, not only a fold or exit 0 |
| Bounded indexing and resume | Partial at required scale | Existing controls/checkpoints; `.44/.46` retain 8.4M-message, cold preparation and throttled-progress obligations |
| Fast useful lexical retrieval | Implemented, full lifecycle unproven | Current archive refuses read-only history query; `.8/.9/.11/.32` cover opening, ownership, deadlines and useful results |
| Single-tier neural/hybrid search | Implemented, broad qualification incomplete | Native model/exact search and fail-open routes exist; real-model, producer, relevance and scale tests remain |
| Native ANN over retained WAL updates | Failing selected acceptance | Three actual admission failures; `ds7uy.3.3` and incremental ANN work retain identity and exact fallback |
| Full progressive two-tier search | Unavailable | `SemanticIndexArtifact` still constructs owner-backed capability false; do not flip the flag without owner-safe constructors and independent quality retrieval |
| Exact search-to-view/pack evidence | Partially validated | Earlier selected GH493 tests passed; `.43/.20/.11` retain full provenance, duplicate-position, changed-source and release cases |
| Persistent low-overhead agent access | Implemented, not fully qualified | `main.rs` dispatches `serve` early; retained lexical reader, MCP, explicit canonical view and bounded candidate reranking exist. Not global semantic retrieval or hard kernel memory admission |
| Logical archive portability | Implemented slices, not fully qualified | `main.rs` dispatches `archive`; export/verify/import/search/view, explicit v20-to-v21 migration and canonical rebuild exist. `.34` needs full original roundtrip, interruption, scale and publication acceptance |
| Freshness, TUI and analytics | Implemented, native/scale evidence incomplete | `.21/.22/.24/.36`; scheduling progress must not confuse politeness with starvation |
| Multi-machine and browser publication | Implemented, live qualification incomplete | `.23/.24/.25`; fixture tests cannot certify native SSH, browser unlock or release delivery |
| Swarm and guided operations | Partial | Passive CASS/proof observations are wired; live lint still uses `render_swarm_lint_live_partial`; eleven guide entries remain `ObservationOnly` |
| Accurate discovery and documentation | Drift confirmed | Schema is 21, README says 20; root introspection omits independent `archive`/`serve`; current swarm/workflow and sync-plan statements lag code. `.26` retains these |
| Tested install/release | Not qualified at current source | `.1/.25`, dependency/release owners and target-specific gates; latest public 0.8.0 does not include every current fix |

### Newly uncovered acceptance and work order

The initial current inventory was **204 unfinished tasks**: 63 open, 115 in
progress, 26 blocked. GitHub now has 59 open issues. Three newly transferred or
filed reports lacked explicit tracker references; GH494 appeared only in a
receipt about incorrectly selected tests. Four dedicated tasks now preserve
their exact obligations:

| Bead | Report and retained acceptance | Existing implementation links |
|---|---|---|
| `.45` | GH495: 253,204-message/~1.3GB orphan-shadow archive; ~78k-message legacy collision, three-row reproducer; classify residue, converge repair, match preview/apply, preserve canonical content | Related `.3`; current absent-corpus recreation bound already exists |
| `.46` | GH496: 19k conversations/8.4M messages/~13GB; 10G swap-thrash, 8G preparation OOM, lost 53% work; useful bounded completion and correct resume | Depends on `.44`; related `.6`; blocks `.11` |
| `.47` | GH497: 4,987 conversations/516,936 messages/~3.9GB; 1,131-row repair followed by writer/reader schema disagreement; truthful deferred integrity and actionable interrupted-artifact remedy | Related `.3/.45`; blocks `.25` |
| `.48` | GH494: 35,799 conversations/3,520,318 messages; fold 1,722 to 12 segments, exit 0 without publication; fresh successful search or explicit failed publication | Related `.7`; blocks `.11` |

`.45` also blocks `.25`; `.11` already leads into release qualification.
Related edges preserve ownership without serializing independent diagnosis.
Suffixes mean `coding_agent_session_search-2l1b0.N`. No implementation task was
closed, reassigned or claimed merely because this audit found it.

**Execution order and granular remaining TODO:**

1. **Usable validation and highest-risk reproduction (small to medium repair;
   original archive cases may require substantial qualification).**
   - [ ] `.1`: enforce every-selector counts, immutable source and executed-binary
     identity; triage strict UBS; correct current fmt/Clippy without peer sweeps.
   - [ ] `.48`: execute the actual GH494 tests; trace each fold/validation/publish
     exit; demonstrate durable publication and fresh search in a new process.
   - [ ] `.45/.47`: reproduce legacy residue/schema cases on disposable copies
     with current dependencies; preserve canonical digests, locks and raw evidence.
   - [ ] `.2/.3`: retain original integrity acceptance; a repaired derivative
     cannot certify an already damaged canonical archive.
2. **Complete existing retrieval and resource paths (medium to large).**
   - [ ] `ds7uy.3.3`: resolve three WAL native-admission counterexamples while
     preserving exact fallback, source-local shadowing and retained ownership.
   - [ ] `.43/.20`: verify positive canonical follow-up and pack identities,
     not just error/refusal behavior or golden shape.
   - [ ] `.44/.46`: bound cold preparation and all concurrent owners; prove
     useful progress under declared limits, valid resume and stale-state rejection.
   - [ ] `.11`: run one complete ingest-to-search-to-view/pack journey at named
     corpus/platform budgets. Separate cold CLI and warm service measurements.
3. **Finish already-built adjacent capabilities (medium qualification).**
   - [ ] `.34`: original logical roundtrip criteria plus actual schema migration,
     identical/conflicting reimport, interrupted no-clobber publication and restore-search.
   - [ ] `.39/.41`: inventory-wide resource and crash-window acceptance; bounded
     output or per-file reads alone are insufficient.
   - [ ] `.18/.19`: compose live observations and wire remaining useful adapters;
     retain explicit mutation permission and unknown evidence states.
   - [ ] `.21/.22/.23/.24/.36`: analytics, TUI, browser, fleet and native scheduler
     journeys on their actual runtime boundaries.
4. **Make real capabilities discoverable and deliver them (small docs work;
   large cross-platform qualification).**
   - [ ] `.26`: current schema, independent command discovery, workflow and
     network/privacy truth; preserve early no-archive startup for `serve`/`archive`.
   - [ ] `.25`: final registry graph, native targets, exact installed artifact
     checksums, browser lane and declared supported capabilities. No release on
     red mandatory gates, no transfer of source-only evidence to released binaries.

### Ambition and refinement outcomes

Applied the existing frozen Phase 3a and Phase 5 prompts verbatim; their texts
are retained in the parent epic rather than copied into another plan.

Three ambition passes improved the actual acceptance:

1. **Evidence conservation:** follow source identity through canonical rows,
   publication, search, view/pack and backup restore. Never equate raw/redacted
   byte hashes or raw file lines with canonical message identity.
2. **Recovery that converges:** demand a positive usable terminal state and
   demonstrate the next new ingestion/search succeeds. Stop counting a truthful
   refusal or a healthy row count as successful repair.
3. **Reuse the working architecture:** qualify `serve` and logical archive
   routes before proposing another daemon/export framework. Measure all owner
   overlap and cold preparation, not only ANN kernels or per-batch bytes.

Five refinement passes:

1. **Coverage:** all 59 open issue references checked; add `.45`–`.47`, then
   recognize GH494's receipt-only reference is inadequate and add `.48`.
2. **Source and proof classes:** schema 21, logical routes and passive swarm
   observations correct stale assessments; preserve unknown original reports.
   Distinguish three ANN admission failures from incorrect returned results.
3. **Tests and ordering:** mixed valid/empty test selectors require individual
   coverage. Related links retain storage/publication owners; actual scale and
   repair qualification feed retrieval/release, not the reverse.
4. **Adversarial/user value:** GH495 legacy-intact/missing-docsize/orphan controls;
   GH497 reopen after repair and truthful skipped probes; GH496 valid/stale resume
   and positive progress; GH494 no-publish outcome plus new-process fresh results.
   Retain capabilities and original acceptance instead of narrowing to easy tests.
5. **Consistency:** reread new task descriptions, dependencies, evidence labels
   and remaining TODOs. The late merge required the GH495 patch/failed-run/gate
   clarification and acknowledgement of newly documented controls above; those
   corrections were added to `.1/.45/.46`. Recheck the graph and tracker export
   afterward. No additional scope change after these corrections. This bounded convergence is not proof
   that every possible defect or historical plan obligation was discovered.

Final graph inventory: 208 unfinished (67 open, 115 in progress, 26 blocked),
2,069 closed, one tombstone retained. `br dep cycles` and `bv --robot-insights`
report no active cycles; `bv --robot-triage` loaded the authoritative SQLite
tracker with no errors/warnings. Git diff whitespace checks pass. These are
tracker/document checks, not product qualification.

**Would finishing the prior backlog deliver the full vision?** Not as defined
at audit start: GH494–497 lacked adequate issue-specific acceptance. The revised
graph covers the identified gaps, but only executing its original criteria can
establish completion. Existing code, closed counts and enabled workflows cannot.

Honesty disposition: this is documentation/tracker work, self-reviewed, with no
new product tests, goldens, gate edits, suppressions, release or performance
claims. The prior zero-GH494 selection is disclosed and encoded in `.1/.48`.
Historical document reads were selective; the two requested governing documents
were read fully. Public issue triage comments were not sent. No original archive
was modified and no files were deleted.

---

## Current assessment: 2026-09-21

This assessment supersedes the current-state claims in the dated sections below.
Those sections retain historical observations, failures and proposed targets;
their dependency pins, workflow state, completion labels and timings are not
current certification.

**Verdict:** cass implements most of its broad feature set, but the dependable
large-archive retrieval promise is not yet established. The immediate priority
is preserving private history and returning the correct evidence within bounded
resources, then delivering that behavior in tested release artifacts. More
features or more closed beads cannot substitute for that result.

### Scope and evidence boundaries

Baseline: local `main` at `4cbcf1f8488faa7693a9b0bdac8e8138c95dffdf`, initially
clean. AGENTS.md (1,329 lines) and README.md (3,564 lines) were read in full.
Investigation traced selected production paths and tests across discovery,
storage, indexing, lexical/semantic retrieval, packs, scheduling, sync, analytics,
Pages, swarm observations and guided operations. Current contracts and the
existing bridge plan were cross-checked; the roughly 23,000 lines of planning
material were inventoried and selectively reviewed, not all reread line by line.
This is a broad product assessment, not an exhaustive source/security audit.

Keep four evidence classes distinct:

1. **Current source:** reachable implementation at the baseline above. A test
   present in source is not a fresh passing execution.
2. **Fresh observation:** read-only installed CLI, GitHub release/workflow APIs,
   Git history, tracker inventory and graph validation during this assessment.
3. **Earlier or reported execution:** receipts and issue reproductions retain
   their exact version, platform and corpus. They are acceptance obligations,
   not automatically reproduced bugs on this HEAD.
4. **Proposed acceptance:** behavior to demonstrate before closing the gap.

No source archive was repaired, indexed or cleaned during this audit. No model
was acquired and no new build was started. Earlier remote gates are described
below without turning them into fresh audit results.

### What has actually changed since the previous assessment

- Latest public release is **v0.8.0**, published September 10, at
  `96510ff5d06f678d25d5c786d058e4b6ea628827`; the checkout manifest says **0.9.0**.
  Installed `cass --version` also reports 0.8.0. Linux x86_64/arm64, macOS arm64
  and Windows x86_64 assets exist; no Intel macOS binary appears in that release.
- Cargo.toml now selects FrankenSQLite **0.4.4**, Asupersync **0.5.0**,
  Frankensearch **0.6.1** and FAD **0.3.0**. Published packages and exact pins do
  not establish CASS runtime safety or repair an already damaged archive.
- Native OpenClaw SQLite is enabled in the normal dependency feature list.
  Codex source containment, exclusions, large-source admission and native
  consumer tests have landed. Do not recreate those implementations because an
  older issue still describes the released binary.
- Semantic artifact retirement and fingerprint-approved offline scratch
  recovery have landed, including real storage-backed test cases. Their full
  correctness, resource bounds and release delivery remain separate questions.
- Live Git/Beads/RCH/Agent Mail collectors exist. CASS swarm evidence now includes
  passive lock observations and proof-metadata inspection; calling all of it
  unwired is stale. Metadata inspection is not executable-proof certification.
- General CI, Browser Tests and Release workflows remain manually disabled,
  but many focused repository workflows are now active. The recent API sample
  includes queued, cancelled, failed and successful runs on different SHAs.
  Neither “all workflows disabled” nor “CI is green” describes this state.

### Architecture and vision coverage

The primary flow is provider discovery/normalization through FAD and CASS-owned
connector seams, source capture and bounded ingestion, then the canonical
FrankenSQLite archive. Quill lexical indexes, semantic vector/ANN generations
and analytics rollups are derived assets. CLI and TUI consumers share search,
hydration and readiness machinery; packs add selection, provenance, redaction
and final output budgeting. Scheduler/daemon work launches child commands under
the indexing lock. Source sync transports histories with provenance; it is not
the proposed lossless logical-archive import/export contract.

The main architectural risk is lifecycle composition: source admission,
database opening, migration, generation ownership, inference, hydration and
maintenance all contribute to a command's latency, memory and correctness.
Optimizing an ANN lookup or lexical kernel alone cannot prove the full journey.

| User promise | Current source and remaining gap | Existing or added coverage |
|---|---|---|
| Find histories across providers | Real FAD integration plus CASS-specific normalization. Native OpenClaw and Codex fixes landed; exclusions, large files and exact published-consumer behavior need qualification. | Provider beads; `.35`, `.37`, `.38`, `.40` |
| Preserve authoritative history | Real storage, raw mirror, recovery plans and writer management. Reported archive damage and exact dependency/recovery qualification remain unresolved. | `scohn`, `.2`, `.3`, `.4`, `55wh0`, `hyydu` |
| Finish indexing honestly | Streaming, source ledger, partial coverage and resume mechanisms exist. One rejected source must not starve later sources or yield false full success. | `.6`, `.7`, `.35`, `.40`, `irqgt`, `gk9ho` |
| Search promptly at real scale | Real Quill lexical search, bounded fuel, filters and self-healing publication. Cold admission, narrow-filter costs, whole-process memory and useful deadlines remain acceptance gaps. | `.8`, `.9`, `.11`, `.32` |
| Follow a result to the right evidence | Current reports show message ordinals confused with raw file lines, including plausible wrong content marked as the target. This threatens the central evidence contract. | New P0 `.43`; `.11`, `.20` |
| Useful semantic/hybrid results | Native MiniLM inference, exact vector search, hybrid fallback and optional ANN are real. Producer identity, late passages, multilingual relevance, restart and scale still need full consumer proof. | `jyfuq.2`, `ds7uy`, `.33`, `mtt5q` |
| Progressive two-tier refinement | The owner-backed progressive-reader capability is still constructed false in `src/search/vector_index.rs`; query routing checks it. This specific path remains unavailable. | `ds7uy.3`, `ds7uy.3.3` |
| Safe semantic generation rollover | Retained-generation design and recovery mechanisms exist. Prior readable evidence must survive backfill, interruption and stale recovery plans without serving deleted/changed content. | `962e8`, `.41`, GH458 contract |
| Stay fresh unobtrusively | OS schedules, detached refresh and bounded backfill exist. macOS priority starvation, busy-versus-fresh reporting and retained work need native qualification. Nightly reconciliation already exists in source. | `.24`, `.36`, GH471/GH472 work |
| Reliable TUI at scale | FrankenTUI implementation is substantial. First frame, memory, responsiveness, terminal behavior and search relevance on large archives remain distinct acceptance obligations. | `.22`, GH395 |
| Compact trustworthy handoffs | Packs are implemented, with citation checks, redaction and final serialized-budget work. The conformance matrix remains partial; its zero fully-certified totals do not mean there is no implementation. | `.20`, `.43` |
| Accurate analytics | Fact/rollup queries and explicit no-data/true-zero/invalid-input outcomes exist. Provider completeness, source-kind classification and rebuild consistency still need full journey proof. | `.21`, `rril3` |
| Multi-machine history | SSH/rsync/SFTP paths and scheduled due-source handling exist. Setup, provenance, transfer accounting and partial failure need end-to-end evidence. Logical archive round-trip is a separate unimplemented contract. | `.24`, `.34` |
| Safe export and browser access | HTML/Pages encryption, verification and publication code exists. Browser unlock, privacy, failure recovery and native publication require actual lanes; browser workflow is disabled. | `.23`, `70o8f` |
| Useful swarm and guided operations | Real bounded observers and some guided adapters exist. Observation-only adapters remain, and composed live acceptance is incomplete. | `.16`–`.19` |
| Bounded storage and honest diagnostics | Quarantine/doctor/retention exist. Preview-ledger growth and semantic scratch retention expose lifecycle gaps; old precious evidence must remain protected. | `.39`, `.41`, recovery beads |
| Safe concurrent writers | Reader pooling and a concurrent-writer API exist; ordinary production writes use BEGIN IMMEDIATE. Multi-writer capability is not a proven production performance benefit. | `.29` |
| Install a version that delivers these promises | Releases and installers exist, but source fixes outpace published artifacts and complete native/browser gates are missing. | `.1`, `.25`, `55wh0`, `aegfi` |

Suffixes `.N` in this section mean `coding_agent_session_search-2l1b0.N`.
Other short IDs use the same project prefix.

### Direct observations and unresolved validation

The read-only installed-history query used lexical mode, workspace scope,
`--no-maintenance`, a two-second command budget and a ten-second outer limit.
It returned **exit 5 / maintenance-required** because the lexical rebuild
checkpoint was incomplete. No results were delivered. The refusal is truthful
and protects the archive; it does not satisfy useful retrieval. This establishes
the state of this configured archive on installed 0.8.0, not universal failure
or a new reproduction of b-tree corruption. The restriction was not relaxed to
trigger repair merely to obtain a successful audit screenshot.

Earlier on September 21, the GH364 remote gate produced `STAGE=fmt EXIT=1` and
`STAGE=clippy EXIT=101`, then an RCH 1,800-second timeout. Tests, goldens and UBS
had no terminal result. The source-stability check also failed after this
agent appended a Bead during the run; the Rust file hash stayed unchanged.
The corrected-timeout submission was refused for capacity before execution.
These are neither a clean gate nor evidence that the proposed Quill regression
test passed. Receipts: `/tmp/cass-rainyorchid-gh364-typed-errors-gate-1.log` and
`/tmp/cass-rainyorchid-gh364-typed-errors-gate-2.log` (local session artifacts).
Earlier selected swarm/service and release tests remain useful narrow evidence;
unresolved strict UBS, excluded scopes and exact dependency reproducers still
prevent a blanket release-clear statement.

The public workflow sample likewise cannot certify this local HEAD: for example
GH473 writer contention succeeded at `d002bfa7`, while semantic recovery failed
at that SHA, and more recent coordinate/retrieval runs were queued or cancelled.
The next verifier must inspect the complete terminal job and its source/artifact
identity rather than transferring a green badge across revisions.

### Coverage finding: would finishing the old backlog finish the product?

**No, not as it stood at the start of this audit.** The inventory contained
2,069 closed and 194 unfinished beads (111 in progress, 26 blocked, 57 open),
while GitHub had 56 open issues. The ten reports below had no dedicated record
in the open or closed Beads descriptions or exported JSONL, despite several
having source fixes. The old v0.8.0 release task remained blocked even though
that release exists. Task-state drift obscures both real progress and missing
acceptance. A closure percentage is not a product completion percentage.

| Report | New issue-specific bead | What was missing from the old graph |
|---|---|---|
| GH484 oversized source aborts later scan | `.35` | Source containment and top-level partial accounting through the real consumer |
| GH485 launchd starvation | `.36` | Native throttled-vs-useful progress and freshness truth |
| GH486 Codex exclusion gap | `.37` | Privacy boundary across scan, explicit source and raw mirror; release proof |
| GH487 native OpenClaw feature omission | `.38` | Default published-consumer and WAL-only update qualification |
| GH488 dry-run ledger growth | `.39` | Preview nonmutation and bounded ledger/storage accounting |
| GH489 permanent large-source rejection | `.40` | Bounded large-source admission and stable/new partial-failure distinction |
| GH490 semantic scratch amplification | `.41` | Durable retirement and fingerprint-approved killed-run recovery acceptance |
| GH491 invalid ANN setup help | `.42` | Executable help/capability command contract |
| GH493 wrong target evidence | `.43` | Canonical message identity across search, view/expand and packs |
| GH483 bounded slow indexing | `.44` | Discoverable coupled resource controls and current-source skewed-corpus progress proof |

Refinement also mapped GH475 explicitly into existing recipe task `.10`, retaining
its distinction between executable downstream guidance and historical quotations.
The final reference sweep found no open GitHub issue without an explicit tracker
reference. This checks traceability, not adequacy or passing acceptance for all
56 reports. The final inventory is 204 unfinished tasks: 111 in progress,
26 blocked and 67 open; the 2,069 previously closed tasks were not bulk-changed.

The broader missing capabilities already have coverage: progressive ownership,
logical archive round-trip, composed live swarm/guided operations and complete
release journeys. Do not create new epics for those. All new beads include
reporter evidence, current-source distinctions, unit/error cases, real E2E
acceptance, resource/provenance logging and publication handoff. Public issue
comments remain a separate handoff; this audit does not claim they were sent.

### Bridge plan and logical execution order

1. **Establish safe evidence and a usable gate.** Existing owners continue the
   strict gate and exact dependency-integrity qualification. Preserve original
   failures and damaged archives. Obtain final-source, terminal RCH receipts;
   do not replace unavailable runtime evidence with registry publication.
2. **Close privacy and wrong-evidence boundaries first.** Qualify Codex exclusions
   (`.37`) and establish exact message identity (`.43`). Investigate in parallel
   with integrity work where paths/owners permit. A confidently wrong excerpt
   is a higher product risk than a truthful refusal or a missing optional tier.
3. **Complete lossless, bounded ingest and freshness.** `.35` precedes `.40`
   acceptance; retain later good sources and completed work under oversized,
   unreadable and changing inputs. Finish scale write/open costs and native
   scheduler behavior (`.36`), without masking fresh failures as success.
4. **Prove the full retrieval loop.** `.11` must include exact follow-up identity,
   filters, restart, absent/stale assets and useful nonempty results at declared
   corpus/platform budgets. Semantic work includes real model relevance,
   independent quality candidates, retained-generation ownership, exact-oracle
   ANN recall and `.41` storage bounds. Hash-only or file-presence checks are
   insufficient for MiniLM or progressive claims.
5. **Complete secondary journeys against the same truth.** Packs, analytics,
   TUI, source sync, Pages and swarm/guide retain their existing tests and scope.
   `.39` constrains previews; `.42` fixes executable operator instructions.
   These can proceed independently of unrelated future optional features.
6. **Qualify and ship exact artifacts.** `.25` records a capability-by-target
   matrix with known omissions, native Windows/macOS behavior, installer/upgrade
   paths and GitHub-only browser proof. A maintenance release may honestly omit
   optional future capabilities; it cannot omit privacy, archive safety or
   evidence correctness from a claim that those core paths are fixed.

Every acceptance run must identify source, executed binary, published dependency
graph, corpus/model/generation, platform, command, counts and terminal exit.
Measure process-tree memory, disk growth and the full command lifecycle. Set
budgets before running on a named corpus; historical universal millisecond/RSS
targets below are not freshly certified SLOs. Performance comparisons require
parity and valid interleaved A/A controls. Correctness results stand separately
when performance has no verdict. Use `RUST_MIN_STACK=134217728`; older Bead
boilerplate specifying 16 MiB is superseded by current AGENTS.md.

### Ambition and refinement record for this refresh

The frozen Phase 3a prompt was applied verbatim in the epic comment. Existing
owned tasks were retained and ten uncovered issue-specific tasks added, including
the additional GH483 gap found during refinement.
Ambition passes and subsequent graph refinement are recorded here as substantive
changes, not as claims that implementation or runtime qualification occurred.

- **Ambition 1 — evidence fidelity:** require identity conservation from provider
  source through archive, search, follow-up and pack; separately test identical
  text at different positions, changed/pruned sources and nonlocal provenance.
  `.43` becomes a prerequisite for full retrieval and pack conformance.
- **Ambition 2 — lifecycle costs:** count overlapping generations, scratch/reuse
  files, raw-mirror ledgers, models and child processes. `.39` and `.41` cover
  disk/preview costs, and `.36` distinguishes polite scheduling from starvation.
- **Ambition 3 — delivery and scope:** separate source fixed, qualified and shipped
  states; require exact target receipts. Preserve optional feature breadth while
  making privacy, source containment and evidence identity core release gates.

The frozen Phase 5 prompt was applied verbatim in the epic comment. Five passes:

1. **Coverage:** scan all 56 open GitHub reports against both unfinished and
   closed descriptions/notes and exported JSONL. Add `.44` for GH483; extend
   `.10` for GH475 instead of duplicating the recipe campaign. Preserve the
   reporter's missing OOM attribution as missing evidence, and reuse GH426's
   source resume ownership rather than inventing another checkpoint system.
2. **Current-source accuracy:** inspect raw-mirror prune code and its dry-run
   test. The test currently requires appended preview records in `pruned.jsonl`;
   `.39` must deliberately change that contract, with output-only summaries and
   preserved applied-action evidence. It is not merely an old-release report.
3. **Dependency direction:** separate pre-publication candidate qualification
   from post-publication installed-artifact verification. Otherwise requiring
   a released asset in a task that blocks release creates a logical cycle even
   when the tracker graph is acyclic. `.25` owns the latter step. Add real
   blocking edges for identity, scan containment and macOS scheduling; keep
   related ownership links nonblocking.
4. **Adversarial acceptance:** add same-size replacement/stale approval,
   symlink/path escape, newly active owner, zero-delta deletion, duplicate text,
   remote/local path collision and redacted-content cases to `.41`/`.43`.
   Require real consumer execution; do not substitute renderer-only fixtures.
5. **Final consistency:** reread the ten new tasks and modified acceptance
   notes, inspect dependency directions and check source/release/evidence labels.
   No further change was needed in this scoped pass. Unknown defects and the
   historical-plan review limit above remain; this is not a completeness proof.

Validation: `br dep cycles --json` reports zero active cycles. `bv --robot-triage`
loaded the current SQLite tracker with complete authority, no warnings/errors,
and 204 unfinished tasks. Its top recommendations include archive integrity
and Windows writer admission, consistent with the safety/release priorities;
its numerical rank does not override ownership or substitute for diagnosis.
All ten new tasks have an external issue reference, concrete tests and E2E
acceptance. JSONL parses, no tracker records were deleted, and `git diff --check`
passes. This audit changes documentation and Beads only; it does not supply a
new Rust, native-platform or browser test pass.

### Answers to the five questions

1. **Working implementation:** real discovery, canonical storage, lexical and
   single-tier semantic search, hybrid fallback, packs, TUI, analytics, sync,
   scheduling, export, diagnostics and several live swarm/guided paths exist.
   Recent source fixes materially improve containment and artifact ownership.
2. **Incomplete or not established:** dependable large-archive operation,
   exact evidence follow-up, exclusion/recovery/release qualification, progressive
   ownership, full pack conformance, logical archive round-trip, composed live
   operations and cross-platform/browser acceptance. Some are missing capability;
   others are implemented paths without sufficient current evidence.
3. **Blockers:** unresolved integrity and lifecycle failures, end-to-end resource
   costs, unavailable/failed validation lanes, source-to-release lag and tracker/
   documentation drift. No single dependency bump clears those boundaries.
4. **Would the initial backlog close everything?** No: ten issue-specific gaps
   and an unlinked downstream propagation request were missing, and stale tasks
   could produce duplicated work instead of user-visible completion.
5. **What now has coverage?** The identified gaps map to the revised existing
   campaign and `.35`–`.44`, with concrete acceptance and ownership links. Completing
   that graph with the specified evidence addresses the identified vision gaps;
   merely closing it, or passing small fixtures, does not prove the vision.

---

## Current assessment: 2026-09-04

This section supersedes the current-state conclusions in the dated September 1
assessment below. The earlier evidence and follow-ups remain useful history;
they are not a description of today's source. This refresh applies the complete
`reality-check-for-project` workflow: investigation, bridge plan, Beads, three
ambition rounds, a Beads update, and five refinement rounds.

### Scope, evidence, and limits

Source baseline: `fe6927706f6dff075d3639a868abd3df08e9e1c7`, clean `main` at
inspection. AGENTS.md and README.md were read in full, together with the project
plans/specifications for ingestion, search, semantic search, analytics, robot
commands, answer packs, swarm operations, recovery, installation, sync, TUI,
Pages, optimization, and their conformance/testing documents. Historical research
and audit recommendations were checked against today's implementation before
being carried forward. This was targeted source investigation across the
production journeys and tests, not a claim to have read every Rust source line.

Evidence classes used here:

- **Source:** reachable current implementation, including deliberately disabled
  routes. Tests found in source are not automatically passing tests.
- **Fresh execution:** the remote gate receipt and isolated installed-binary
  smoke described below. Their source versions and coverage differ.
- **Reported:** issue reproductions and earlier archive experiments, with their
  reported version/corpus. They establish unresolved acceptance obligations,
  not a fresh reproduction of the same defect on this HEAD.
- **Proposed:** targets, experiments, and architecture decisions still to prove.

Initial `br` database inventory was 2,146 issues: 2,046 closed, 46 open, 49 in progress,
5 blocked. Thus 95.3% closed is a measure of tracker throughput, not 95.3% of the
product promise fulfilled. Existing assignees and reservations are preserved.
GitHub had 23 open issues; all 12 repository-defined workflows were manually
disabled (two GitHub Copilot workflows were active). Latest GitHub release was
v0.7.1, published August 31. Its assets cover Linux x86_64/arm64, macOS arm64,
and Windows x86_64; an Intel macOS binary was not among those assets. Package
registry/Homebrew/Scoop versions from the older report were not revalidated.

### Where cass really is

cass is a substantial implemented product, with real discovery, persistence,
lexical retrieval, semantic machinery, robot contracts, TUI, analytics, sync,
and export code. It is **not yet demonstrated to be a dependable, inexpensive
history-retrieval tool across the large archives reported by its users**.
The largest gap is the cost and reliability of the entire command lifecycle:
archive admission, migration, index opening, model/vector ownership, query,
hydration, serialization, and any spawned maintenance. A millisecond search
kernel does not make a minute-long CLI command fast.

The September 1 assertion that semantic search "does not exist" is too broad.
Native single-tier MiniLM inference and semantic retrieval are real. The specific
unimplemented contract is safe owner-backed **full progressive retrieval**:
`SemanticIndexArtifact::owner_backed_progressive_reader` is constructed false,
and query routing checks that capability. The current README acknowledges the
inactive two-tier route. Likewise, today's bounded robot search workers, repair
circuit breaker, FTS budgets, and Quill publication controls already exist;
the plan must validate and finish those mechanisms, not recreate them.

### Vision and implementation matrix

"Implemented / unproven" means real code was traced but the complete journey
was not freshly demonstrated in this audit. It does not mean the feature is a
stub or that its existing tests fail.

| Goal and measuring stick | Current status and evidence | Remaining obligation / existing coverage |
|---|---|---|
| Discover and normalize diverse local histories (README, original plan) | Implemented: FAD 0.2.2 re-exports plus CASS-specific Codex/OMP seams | Real provider fixtures, provenance, incremental changes; Shelley/OMP/Kiro/Prime already have Beads; Freebuff/GrokBot/Devin requests lacked dedicated coverage |
| Preserve the authoritative archive (AGENTS search-asset contract) | Implemented storage; reported integrity failures remain critical | `scohn`, `9lz4y`, `g3zyo`, `iify0`; distinguish real b-tree damage from FTS validator disagreement |
| Use FrankenSQLite throughout production | Implemented migration boundary; rusqlite is a dev dependency at this HEAD | Keep legacy test oracles separate; no new rusqlite code; do not describe reader pools as proven concurrent transactions |
| Exploit safe concurrent writers (historical integration vision) | Partial: manager has `concurrent_writer`, but ordinary transactions use BEGIN IMMEDIATE; no BEGIN CONCURRENT SQL found | Retain archive integrity as prerequisite; establish whether concurrent writers improve the measured workload before changing the single-writer contract |
| Complete indexing with useful progress (README/recovery) | Implemented pipeline and checkpoints; scale acceptance unresolved | GH450 hidden 1,491-second repair and GH443 checkpoint classification; `cjugu`, `fyepq`, `iify0` |
| Heal derived lexical assets without losing source data (AGENTS) | Implemented scratch build, publish, retention/recovery; Quill auto-sealing disabled | Crash/failure/platform tests and growing-archive measurements; do not recommend deleting index directories |
| Retrieve lexical results quickly (README/performance plans) | Real Quill facade, bounded query fuel, schema identity and fallback | Full cold CLI latency/RSS, filters, hydration, and aggregate costs; GH452, `u3vho` |
| Prefer hybrid and fail open truthfully (AGENTS) | Implemented mode metadata and budget-aware setup; explicit semantic remains distinct | Assert lexical-first useful output within budget, producer validation, no hidden work after timeout; `ds7uy.4.1` and fleet `.2.6` |
| Meaningful native semantic results (semantic plan) | Implemented pure-Rust native MiniLM and multilingual model spaces | Real model relevance, restart, topology/producer mismatch and release proof; `jyfuq.2`, `ds7uy`, `wfm4e` |
| Progressive quality-tier retrieval (semantic plan) | Partial: owner-backed capability deliberately false | Upstream owner-accepting API, pinned generation, independent quality retrieval and E2E; `ds7uy.3` |
| ANN acceleration with honest exact fallback (README) | Implemented optional native ANN admission; readiness/file presence is weaker than search proof | Cold global asset cost, multi-shard correctness, exact-oracle recall; `ds7uy.3.3`, `wfm4e` |
| Reuse semantic work through daemon (README) | Implemented embedding/reranking protocol and attestation | Protocol has no search request: a warm model daemon does not imply warm vector/search ownership |
| No implicit model download (AGENTS) | Implemented explicit install/from-file design | Preserve across TUI, retry, daemon, upgrade and failure; old automatic-download plan is superseded |
| Agent-safe bounded JSON/JSONL/TOON (robot plan) | Implemented contracts/goldens and robot budget machinery | End-to-end deadlines, memory, output volume, malformed input, nonempty test receipts; 131 KB capabilities output merits a compact discovery route |
| Useful answer packs and citations (answer-pack contract) | Implemented planners/CLI/renderers; conformance document still says zero tested | Refresh each MUST/SHOULD row from executed assertions, retain redaction/omissions/evidence provenance |
| Truthful health, status and doctor (AGENTS/recovery) | Implemented bounded/read-only paths; isolated status works | Large archive first-contact and multi-row integrity proof; `k2k20`, `9lz4y`, `kupq4` |
| Freshness without maintenance storms (AGENTS/schedule) | Implemented detached refresh, schedule, locks, breaker and resource gates | Success after failure, cooldown reset, child accounting and native platform scheduling; `iify0` |
| Responsive, polished TUI (TUI specs) | Substantial ftui app and headless/PTY tests | Real first frame, navigation/resize and semantic cancellation on large corpus; GH395 remains acceptance debt |
| Accurate bounded analytics (two analytics plans) | Real rollup-first queries and ledger paths | Filtered scale, missing ledger, mixed estimates/API usage, overflow/time boundaries; avoid misclassifying GH452 as an analytics report |
| Multi-machine source authority (sync plans) | Implemented SSH discovery/sync and source mapping | Interrupted sync, mirror identity, source loss and native Windows path proof; existing fleet and `zp0fp` work |
| Safe HTML/Pages export (Pages plan) | Real filtered export, guard, staged DB image, encryption/publication machinery | Browser tests are not currently CI-backed; memory, key lifecycle, privacy and concurrent publish proof; `70o8f`, privacy Beads |
| Live swarm cockpit (swarm contract) | Partial: fixture adapters/derived views; live status deliberately reports `live-provider-unimplemented` | Real bounded git/Beads/Mail/RCH/CASS readers and failure-isolated integration under `oh96l` |
| Guided safe operations (guided-ops plan) | Partial: support-capsule route is implemented; other adapters explicitly unavailable | Wire typed preview/apply/verify/compensate through existing commands; retain explicit operator mutation contract |
| Reliable install/update across targets (installer/release docs) | Shipped release and scripts; standalone Rust installer plan not fulfilled | Target-specific installed-binary acceptance, Windows update regression, release evidence; `yviq2`, `aegfi`, `4w0ma` |
| Full JSONL archive portability (SYNC_STRATEGY) | Historical proposal; current SSH/source sync is a different capability | Record a deliberate scope decision or implement schema-versioned logical portability with source identity; do not silently count SSH as completion |
| Reproducible blocking quality gates (AGENTS) | Real RCH gate; fmt and clippy have fresh receipts | Gate does not invoke UBS; need nonzero test counts, immutable source/binary identity and native/browser receipts |
| Maintainable shared development (AGENTS) | Reservation/guard discipline exists; giant shared modules persist | Extract narrow proven seams only as needed; six examined files total 292,303 lines including tests, increasing review/conflict cost |

### The user reports that change the priorities

- [GH452](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/452):
  roughly 1.15 million messages. Native ANN lookup reportedly took 2.07–2.45 ms,
  but the full commands took 66.52 and 79.40 seconds with approximately 18 GiB
  RSS. Explicit lexical took 3.22 seconds and 3.095 GiB. A narrow session filter
  still admitted global semantic assets. These are reported v0.7.1 measurements,
  not measurements of this HEAD. Current robot setup already uses bounded
  read-only workers; thread timeout alone does not establish a memory bound or
  cooperative cancellation. Extend existing budget work and measure the delta.
- [GH450](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/450):
  425,321 messages, 2,842,697,904-byte bundle; pre-index v8→v9 compatibility repair
  took 1,491.1 seconds with misleading stalled/zero progress and high I/O pressure.
  This is a hidden expensive phase, not automatically the GH413 deadlock.
- [GH443](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/443):
  an 8 GiB macOS host with a 1,801,728,000-byte database stalled classifying a
  completed stale checkpoint; current corpus 1,840 conversations/386,009 messages
  versus checkpoint 1,557/356,129. Small successful fixtures do not close this.
- [GH391](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/391):
  recurring real rowid-order damage, also detected by stock SQLite, after normal
  indexing. Keep separate from [GH438](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/438)
  FTS structural disagreement and [GH402](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/402)
  a binary-only read-only open regression on an unchanged archive.
- [GH381](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/381)
  and [GH379](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/379):
  count/open amplification and first contentless-FTS mutation amplification,
  respectively. The latter reported 66 GB RSS plus 24 GB swap on a 2.9 GB DB.
  `iify0` already covers the CASS FTS budget/shadow policy; attach these exact
  obligations instead of making a second competing implementation task.

The latest fsqlite pin is **0.3.16**, carrying important WAL-tail and FTS fixes.
That is material progress, but consuming a fixed dependency is not equivalent
to successful archive acceptance. The earlier September 4 evidence below records
existing archive damage and `scohn`; this audit did not mutate or run repairs on
the owner's live archive. Archive-scale comparisons must use authorized,
quiescent copies and retain structural evidence before any repair experiment.

GH452 also records material progress at pinned `main`
`90607c6f22ba9d153d784c8ce5680e3e40c418e1`: a separate default-hybrid comparison
fell from 104.98 s/16.05 GiB on v0.7.1 to 40.75 s/7.39 GiB with identical first
50 result IDs/order. Preserve that reported window independently; the causal
change was not isolated, and it is not this audit's HEAD. In particular the
strict `--no-maintenance` route did not exercise live fingerprint computation,
so the improvement cannot be attributed to the fingerprint cache alone.
Path-faithful isolated copies matter because relocated archive paths can fail
checkpoint admission and accidentally measure a different route.

The same report isolates duplicate Quill opening. Current source still validates
through `search_lexical_read_only_diagnosis`/`validate_searchable_index_contract`
and separately opens `SearchClient`'s reader. Bead `.32` below retains the valid
reader and its trust checks; it must not ship the diagnostic validation bypass.

### Bridge plan and sequencing

1. **Integrity and evidence first.** Preserve/reproduce the separate damage and
   interoperability classes; repair the proof gate; get bounded stage/corpus
   receipts. Existing P0 owners retain their tasks. New issue-specific work adds
   missing acceptance and reproducer coverage.
2. **Finish the ordinary retrieval loop.** Reuse current robot budgets and
   `u3vho`/fleet work. Bound admission, allocations and child work; make checkpoint
   and migration phases observable; prove lexical results remain useful while
   semantic refinement is unavailable, late or too expensive.
3. **Deliver the semantic promise on that foundation.** Complete `ds7uy` immutable
   owner API integration and native MiniLM evidence. Profile startup separately
   from ANN lookup. Choose query-context reuse only after a measured comparison
   with bounded per-command opening; do not assume adding a daemon is the fix.
4. **Finish supported journeys, not isolated handlers.** Exercise TUI, packs,
   analytics, sync/scheduling, export, installation and upgrade through real
   binaries and their native platform boundaries. Preserve the requested feature
   breadth while putting release-critical retrieval/integrity work first.
5. **Wire genuinely absent integrations.** Add requested provider support in FAD,
   and live swarm/guided-operation adapters in their existing CASS modules.
   Fixture projections and explicit unavailable states stay honest until wired.
6. **Make completion durable.** Map every promise and GitHub issue to a Bead and
   executable acceptance; reconcile stale migration tasks without stealing
   ownership. Publish only release claims demonstrated by the exact binaries.

For each implementation Bead, record problem/report, current source seam, scope,
dependencies, units/edge/errors, real-binary E2E, and terminal evidence. New work
uses FrankenSQLite, opt-in model acquisition, no source deletion, read-only
inspection by default, source reservations, and the RCH batched gate. Browser
E2E stays on GitHub Actions; disabled workflows yield missing evidence, not a
local-browser exception. Publishing and external issue comments are separate
operator actions, not side effects of this audit.

### Acceptance design

Use a matrix of empty/small, approximately 400k, 1.2M and 2M-message corpora;
small and large WALs; healthy, damaged and missing derived assets; cold and warm
processes; model absent/present; narrow and broad filters. Generated structural
fixtures must contain realistic size/skew/long messages and record what they
cannot represent. A model-free fixture cannot establish MiniLM relevance.

Primary retrieval acceptance is useful, correctly scoped, cited evidence within
the caller's deadline and declared memory budget. Record wall time from process
spawn, stage durations, peak process-tree RSS, I/O, result count, partial/fallback
reason, source/ELF hashes, corpus/model/generation identity, platform and terminal
exit. A fast empty partial response is a safety result, not retrieval success.
Targets are corpus/platform-specific and proposed until measured; old universal
60 ms/300 ms or 500 MB–1 GB claims cannot substitute for such receipts.

Index acceptance includes monotone meaningful phase progress, bounded memory,
interruption/restart, publication crash points, reader coexistence and independent
archive validation. Preserve source authority and old generations on failure.
Do not require WAL truncation while a legitimate reader pins its generation.

Release acceptance includes exact installed artifact checksums, dependency/source
identity, Linux/macOS/Windows behavior, bounded upgrade/read-only first contact,
offline model absence, successful lexical retrieval, and truthful unavailable
optional services. Test failures, skipped/no-test selections, missing browser or
native target runs, refused fleet admission and stale artifacts remain explicit
non-passes. Development-source evidence is not released-binary evidence.

### Fresh verification ledger

- Installed `/home/ubuntu/.local/bin/cass`: v0.7.1, build `19336ea7e992`, SHA-256
  `3414e60f3a62345e80cfcb65023b41c749f02c52278cbcb369fca3260918d8fc`.
  Isolated HOME/XDG/provider roots and data directory, auto-refresh disabled.
  `api-version`, `selftest`, `capabilities`, `swarm status`, support-capsule dry
  run and empty-dir `status` all returned JSON and exit 0 (7–53 ms wall).
  Selftest explicitly reported `archive_accessed=false`; live swarm reported
  `partial=true` and `live-provider-unimplemented`. These are limited positive
  proofs of those contracts, not archive-scale or full swarm success.
- Smoke receipts and complete command output:
  `/data/tmp/cass-reality-20260904-smoke-owerbubk/receipt.json` and sibling files.
  These are local retained artifacts, not promised permanent release URLs.
- A second, fresh installed-release fixture run completed real-format Codex
  ingestion → lexical search → view → pack. Index: 827 ms; search: 16 ms and
  three hits; view: 9 ms; valid pack: 29 ms and three evidence items. Twelve
  explicit assertions passed, including expected source identity, lexical mode,
  invalid pack-limit rejection, positive results and unchanged fixture bytes.
  Receipts/output/assertions: `/data/tmp/cass-reality-core-20260904-pd898ocy/`.
  This is a tiny functional smoke with two-CPU affinity during indexing/search,
  not a performance certification or proof of every citation/privacy invariant.
  The initial `--max-tokens 800` pack request correctly failed with
  `pack-invalid-limit` (minimum 1024); the valid request used 2048.
- An earlier fixture attempt under a 2 GiB **virtual address space** limit aborted
  during Rayon thread-pool creation (`Resource temporarily unavailable`). Its
  receipt remains in `/data/tmp/cass-reality-core-20260904-vgjqfqjm/`. This is
  not an RSS measurement or an archive-scale failure; the successful run bounded
  CPU/thread concurrency and removed that address-space cap. Preserve both facts.
- Current-source remote gate: worker `vmi1149989`, baseline HEAD above,
  `scripts/gate.sh --lib-filter 'search::quill_bridge::tests' --integration
  cli_robot,bookmarks_cli --docs-truth`, `RCH_REQUIRE_REMOTE=1`, one admission.
  Receipt: `/tmp/cass-reality-20260904-gate-receipt.log`; full orchestration log:
  `/tmp/cass-reality-20260904-gate.log`. Final result at 21:32:21 UTC: RCH-E104
  SSH timeout after 30 minutes, `STAGE=rch EXIT=1`, gate RED.
  `STAGE=fmt EXIT=0` and `STAGE=clippy EXIT=0` are present. Lib tests,
  `cli_robot`, `bookmarks_cli`, docs-truth, goldens and job-complete all have
  `EXIT=missing`. Their verdict is **unverified**, not pass or demonstrated
  assertion failure. No local cargo fallback was used. This selection would
  not constitute the full library/semantic suite even if it had completed.
- Scoped UBS v5.3.13 on the changed Markdown/JSONL files exited 3:
  no supported languages, nothing scanned. This is **not a scanner pass**;
  the actual changes are documentation/tracker data. `.github/workflows/
  ubs-version.txt` currently contains `latest`, so the gate work must establish
  an immutable version as well as invoke the scanner. Log:
  `/tmp/cass-reality-20260904-ubs.log`.

### Workflow execution record

Initial Bead generation: epic `coding_agent_session_search-2l1b0`, with these
self-contained children (the suffixes below share that exact epic prefix):

| Children | Scope |
|---|---|
| `.1` | Enforced UBS, nonempty tests, source/binary gate receipts |
| `.2`–`.7` | GH391 integrity, GH438 FTS interop, GH402 upgrade reads, GH381 counts, GH450 migration, GH443 checkpoint classification |
| `.8`–`.11` | GH452 memory/deadline admission, semantic startup ownership, agent recipes, full retrieval acceptance |
| `.12`–`.15` | Freebuff, GrokBot, Devin local and explicit Devin cloud acquisition |
| `.16`–`.19` | Live local/service swarm readers, composed swarm acceptance, guided adapters |
| `.20`–`.24` | Packs, analytics, TUI, Pages, sync/scheduling acceptance |
| `.25`–`.30` | Release artifacts, documentation, historical portability/installer decisions, measured module seams, concurrent-writer decision, tracker reconciliation |
| `.31`–`.32` | GH379 first-mutation regression witness; GH452 validated Quill reader reuse |

Blocking edges express actual prerequisites (for example local Devin before
cloud acquisition, both live provider groups before swarm acceptance, and gate/
resource work before full retrieval proof). Related edges retain existing
implementation ownership; they do not falsely serialize independent investigation.
Each child carries background, implementation scope, unit/edge/error coverage,
real E2E requirements, logging/provenance and the relevant safety constraints.

#### Ambition round 1 — completion at the user boundary

The initial graph still allowed a feature handler to pass while the release
journey failed. Strengthen acceptance to distinguish four independent outcomes:
archive safety, bounded response, useful correctly scoped evidence, and exact
released-artifact behavior. A timeout returning empty JSON can satisfy bounded
response while failing retrieval. A release manifest must enumerate advertised
capabilities and their target-specific evidence, including deliberately unavailable
optional integrations; no missing lane becomes a green release claim. Make
integrity acceptance and actual Windows admission explicit prerequisites of the
release proof. Preserve feature breadth in the campaign without requiring every
optional future feature to delay an otherwise honestly scoped maintenance release.

#### Ambition round 2 — account for live ownership and lifecycle cost

Bound resources before allocating, not after a slow operation returns. Establish
an allocation ledger for vector bytes, IDs, filter maps, graph, model, hydrated
results and overlapping generations. For scale intuition only, 1.15M vectors ×
384 dimensions × 4 bytes is about 1.77 GB of raw f32 coordinates before metadata;
this arithmetic is not an RSS measurement. Measure actual simultaneously live
owners, copies and mappings, and compare them with the declared memory budget.
Scope filters must constrain admissible metadata work where possible; a small
result limit must not be advertised as a global asset-memory limit.

Generation-pinned lazy readers, upstream owner-accepting constructors and bounded
query-context reuse are candidate mechanisms. Require a measured choice, explicit
eviction/cancellation and exact producer/corpus identity. Do not activate the
disabled progressive path by changing a boolean: independently retrieve quality
candidates, verify union/fusion and preserve owners across publication. Treat
compaction and migration as named, budgeted maintenance with progress and crash
recovery. Preserve Quill's generation/publication guarantees and the source
archive; do not add automatic destructive cleanup.

Add a dedicated GH379 regression witness linked to `iify0`: the implementation
already exists, but the original first-mutation amplification report needs its
own reproducible acceptance and upstream-versus-consumer attribution.

#### Ambition round 3 — replace optimistic evidence with falsifiable invariants

Use simple formal invariants where they prevent expensive mistakes. Across
indexing/restart, the union of admitted source records must match persisted
logical records under the documented dedup/update policy; source origin must
survive sync and export. Allocated, free and reserved pages must remain disjoint.
A held search generation cannot change identity under a reader. Filtered ANN
recall must be compared with exact retrieval over the same eligible corpus and
embedding producer; global recall does not establish filtered recall. Quality
retrieval must be able to introduce documents absent from the fast candidate set.

Performance acceptance uses controlled interleaved baseline/candidate runs with
separate cold and warm populations, predeclared resource/latency targets, an A/A
control and retained raw samples. Report paired effects and uncertainty; host
pressure, drift, parity failure or invalid controls mean no verdict. Decompose
total latency into measured stages and evaluate the dominant fraction before
optimizing a kernel. Keep memory and latency tradeoffs visible instead of
compressing them into a single "faster" score. This is experimental design for
future measurements, not a certified speedup from this audit.

Preserve separate verdicts for correctness, boundedness, relevance, performance
and release coverage. Tests should be capable of falsifying each contract: wrong
producer with equal dimensions, query generation replaced during open, a timed-out
worker retaining memory, a missing service reported as zero, a stale ELF, a test
filter matching nothing, or a golden regenerated to hide unintended drift.

#### Five refinement rounds and final graph validation

The exact frozen Phase 3a and Phase 5 instructions are retained verbatim in epic
`coding_agent_session_search-2l1b0`. Phase 3a was applied initially and again
after the three ambition revisions; all tracker changes used `br`.

1. **Evidence and ownership:** reviewed every child against current source and
   existing work; separated the GH379 witness and duplicate Quill-open seam;
   retained the GH452 pinned-main improvement without unsupported attribution.
2. **Dependencies:** inspected actual exported edges. `br dep add` did not change
   an existing related edge into a blocking edge, so the campaign's Windows and
   release-path relationships were explicitly corrected. Added actual prerequisite
   edges for retrieval and supported release journeys; independent investigation
   remains startable.
3. **Tests:** strengthened real positive/negative retrieval, source/citation
   checks, Unicode/token boundaries, native model relevance, guided-operation
   postconditions and missing-terminal gate handling. Injection tests establish
   failure handling; realistic execution establishes scale behavior.
4. **Operational constraints:** distinguished RSS from address-space reservations,
   added aggregate client admission, preserved native/browser execution boundaries,
   privacy and explicit mutation contracts, and retained unmet historical feature
   goals when deciding whether an old mechanism is superseded.
5. **Final consistency review:** no further task-definition changes required.
   All 33 campaign issues (one epic, 32 children) remain open/unassigned, with
   self-contained descriptions; no implementation or owner acceptance was falsely
   closed. Existing `iify0` received an additive GH379 evidence note only.

`br dep cycles --json`: zero active cycles. Campaign graph: 32 parent-child,
22 blocking and 21 related edges; every dependency target exists.
The pre-sync `bv --robot-triage` completed successfully with 133 unfinished
issues (79 open, 49 in progress, 5 blocked), with 113 actionable according to bv.
These counts are inventory, not authorization to claim peer work. Its prominent
recommendations include immutable semantic witnesses/manifests and existing
large-archive work, consistent with the bridge priorities. Artifacts:
`/tmp/cass-reality-20260904-campaign-round5.json` and
`/tmp/cass-reality-20260904-triage-final.json`.

Final normal synchronization exposed pre-existing DB/JSONL drift: export refused
because the DB lacked two issues already present in the tracked JSONL. A normal
`br sync --import-only` (no force, deletion or repair) preserved them and refreshed
15 records. `br sync --flush-only` then reported nothing to export. The authoritative
JSONL now contains 2,182 records including one tombstone: 2,051 closed, 76 open,
49 in progress and 5 blocked. Thus the final unfinished count is **130**, rather
than the stale DB snapshot's 133. This changes inventory, not campaign scope.
Final synced artifacts use the suffix `final-synced.json` in the same `/tmp`
directory; cycle and dependency-target checks remain clean. Only the 33 new
campaign records and the additive `iify0` note differ from HEAD in the tracker.

Completing the **initial** unfinished Beads would not have closed all identified
gaps: live integration, untracked user reports, full-command cost and exact-release
proof were missing or insufficiently specified. The revised graph covers those
identified gaps while preserving the existing roadmap. Unknown defects remain
discoverable by its acceptance tests; a comprehensive plan is not proof that no
other defects exist. This assessment implements the steering workflow, not the
follow-on production work.

---

## Historical assessment — 2026-09-01 (superseded where noted above)

> Status: living plan document. Revise **in place**; do not fork copies.
> Evidence base: README.md + AGENTS.md read in full; six read-only code audits
> (semantic, lexical/Quill pipeline, storage/unsafe, TUI, robot surfaces,
> peripheral subsystems) at HEAD `96903b25`; live probes of the installed
> binary (0.6.26) against the owner's real 10.3 GB archive; `.beads/issues.jsonl`
> (2,115 beads); GitHub issues/releases/workflows; git history.
> Line numbers cite the working tree at `96903b25`.

---

## 0. Bottom line

cass is a real, shipped tool whose **core loop works**: discover sessions from
26 agent harnesses (via franken_agent_detection) → persist to frankensqlite →
build a Quill lexical index → answer robot-mode queries with forgiving syntax →
drill down with `view`/`expand`/`pack`. It has 541K lines of Rust, ~10,000
tests, a v0.7.1 release with binaries for every platform, crates.io 0.7.0,
Homebrew and Scoop at 0.7.1, and 2,006 of 2,115 beads closed.

It does **not** deliver the vision the README sells, in four specific ways:

1. **The flagship differentiator (semantic / two-tier progressive search) does
   not exist at runtime.** Three hardcoded `false` gates make every progressive
   lane unreachable; `--two-tier` silently collapses to single-tier; the ANN
   sidecar is never built by backfill; `hnsw_ready` is `path.is_file()`; the
   9K-line generation manifest has no production writer; no test in the repo
   loads a real MiniLM model. The owner's own machine has never installed the
   model. In practice "hybrid" = lexical with truthful fallback metadata.
2. **Reliability at scale is the dominant user-facing failure and it is not
   tracked.** All 22 open GitHub issues are large-archive wedges, hangs, OOMs,
   or corruption. Three were filed today against yesterday's release (#439,
   #440, #441) and none of #439/#440/#441/#395/#391 has a bead. Quill segment
   count grows monotonically on append-only archives (compaction only runs in
   watch mode), so natural-language queries start failing and get worse daily.
3. **There is no quality gate.** Every GitHub workflow is `disabled_manually`;
   the last CI run (2026-08-20) was red. The lib suite has 3 failing tests on
   main. The 3,121-test integration suite has no full-run receipt. The UBS
   "blocking pre-merge gate" described in AGENTS.md is not operating.
4. **The README is materially wrong in dozens of places and silent on ~50K
   lines of shipped code.** Schema v5 (real: v20), dedup mechanism, ~40% of
   keyboard bindings, bookmarks (dead module), "BEGIN CONCURRENT multi-writer"
   (never issued), exit-8 partial results (never produced), swarm live
   composition (fixture-only), sub-60ms (86 ms engine + 1.1 s preflight),
   256-d fast tier (384-d). Meanwhile `cass pages` (45K lines, real deploy)
   and 14 other subcommands have zero README mentions.

Structural debt is growing: `src/lib.rs` is 119K lines (the April
modes-of-reasoning report flagged 111K across *five* files as the #2 risk);
~4.4K lines of the indexer/search are dead Tantivy-only code behind a hardcoded
`None`; five modules are dead or test-only. The bead ledger no longer maps
reality: 21 of 67 in-progress beads untouched >30 days, ~14 are code-complete
but unclosed, 3 are obsolete, and `br show` reports DB/JSONL divergence.

**Completing every open and in-progress bead would not close the gap.** See §5.

---

## 1. Numbers that frame the picture

| Metric | Value | Source |
|---|---|---|
| Rust source lines / files | 541,218 / 245 | `find src -name '*.rs'` |
| Largest files | lib.rs 119,409 · indexer/mod.rs 58,298 · ui/app.rs 47,727 · storage/sqlite.rs 36,442 | wc |
| `#[test]` fns (src + tests/) | 6,885 + 3,121 | rg |
| `#[ignore]`d tests | 104 (31 "Tantivy-only, disabled on Quill", 12 Docker, 8 need real model) | rg |
| Compile state at `96903b25` | `cargo check --all-targets` green in 14 m 11 s (rch worker hz4, admitted on 4th try); 2 unused-import warnings in test targets `logging` and `e2e_error_recovery` would fail `clippy -D warnings`; nix 0.28 future-incompat | scratchpad/cargo-check-3.log |
| Lib suite today | 6,826 pass / 3 fail / 38 ignored | bead bet45, commit 72c069ef |
| Beads | 2,115 total · 2,006 closed · 40 open · 67 in_progress · 1 blocked | issues.jsonl |
| In-progress beads stale >30 d | 21 | issues.jsonl updated_at |
| Open GitHub issues | 22 (3 filed 2026-09-01 against v0.7.1) | gh |
| GitHub workflows | 12 of 12 `disabled_manually`; last run 2026-08-20 (failure) | gh workflow list |
| Commits: week 35 / last 7 d / since v0.7.1 | 396 / 254 / 55 | git log |
| Releases | v0.7.1 (2026-08-31, all platforms) · crates.io 0.7.0 · brew/scoop 0.7.1 | gh, crates.io |
| Installed binary on this host | 0.6.26 (lags main by 100+ commits) | `cass --version` |
| Owner archive | 10.3 GB DB (+10.3 GB engine `pre-migration-bak`) · 1,012 conversations · 538,807 messages · 12 GB raw-mirror | stat, `cass stats` |
| Owner index freshness | last indexed 2026-08-14 (18 days stale); semantic model never installed | `cass health --json` |
| Robot search wall-clock (warm, 3 queries) | 1,132–1,154 ms; `search_ms` 86, `other_ms` 1,080 | `--robot-meta` |
| Non-test `unsafe` sites | 12 (2 env, 10 FFI); 0 `unsafe impl Send/Sync` | ast-grep census |
| Error kinds | 93 (README says ~50) | cli_error_kind.rs |
| Schema version | 20 (README says 5) | sqlite.rs:3709 |

---

## 2. Vision checklist

Status vocabulary: WORKING · PARTIAL · STUB · UNPROVEN · NOT_STARTED ·
REGRESSED · DEAD_CODE · WRONG_DOC (code is fine, README is wrong) ·
NO_BEAD (no open/in-progress bead covers the gap).

### 2.1 Core loop

| # | Goal (README) | Status | Evidence |
|---|---|---|---|
| 1 | 26 connectors normalized via franken_agent_detection | WORKING | FAD 0.2.1 ships 26 connector modules; src/connectors/ is thin re-exports + codex/omp/pi overrides (mod.rs:233-284). AGENTS.md table lists 24 (missing goose, muse) — WRONG_DOC |
| 2 | frankensqlite is the only production storage | WORKING | rusqlite is dev-dep only (Cargo.toml:216-218); two `#[cfg(test)]` call sites |
| 3 | "BEGIN CONCURRENT / MVCC multi-writer" | WRONG_DOC | zero `BEGIN CONCURRENT` SQL; parallel persist env-gated off (`CASS_INDEXER_BEGIN_CONCURRENT`, mod.rs:28739); production tx is `BEGIN IMMEDIATE`; `FrankenConnectionManager` serves one doctor probe (lib.rs:62360) |
| 4 | Quill BM25 lexical index, edge n-grams, smart tokenization | WORKING | `TantivyIndex.inner: QuillCassIndex` (tantivy.rs:1447); schema hash from frankensearch |
| 5 | Atomic-swap publish, retention, crash recovery | WORKING | mod.rs:20225, 20614, 20638; incremental commits go through Quill manifest publish, not dir swap (PARTIAL on "every") |
| 6 | Schema-hash self-heal, rebuild from SQLite | WORKING | tantivy.rs:1406; mod.rs:14818-14838; lib.rs:26906 |
| 7 | Stale-on-read auto refresh, `--no-maintenance` | WORKING | lib.rs:21584; background_refresh.rs:36; lib.rs:667-671 |
| 8 | `cass schedule install` launchd/systemd + nightly + idle gates | WORKING | schedule.rs (born 2026-08-27); not in 0.6.26 |
| 9 | Watch mode 2 s / 5 s debounce, `--watch-once` | WORKING | mod.rs:25992-25993, 9445 |
| 10 | Per-source ingest ledger / resumable incremental (GH#426) | NOT_STARTED | bead fyepq open; refresh_ledger.rs (2.4K lines) tests-only |
| 11 | Append-only messages + BLAKE3 (role+content+ts) dedup + conversation fingerprint | WRONG_DOC | key is `UNIQUE(conversation_id, idx)` (sqlite.rs:6242); BLAKE3 content-only, in-memory; production `DELETE FROM messages` at :7227/:9665 |
| 12 | Schema migrations v1–v5 | WRONG_DOC | `CURRENT_SCHEMA_VERSION = 20`; V18–V20 unbounded single-tx rewrites; engine backup blind to free space |
| 13 | Index-time secret redaction | WORKING | redact_secrets.rs:340-462 |
| 14 | Raw mirror 4 MiB chunks, prune with `--apply` | WORKING | raw_mirror.rs:13-20, 638, 1105, 1249 |

### 2.2 Search quality and performance

| # | Goal | Status | Evidence |
|---|---|---|---|
| 15 | Sub-60 ms search | PARTIAL / UNPROVEN | engine `search_ms`=86 on 10 GB archive; one-shot robot call 1.15 s (`other_ms` 1,080 = DB open + integrity preflight); no latency assert anywhere; TUI debounce is 8 ms (doc says 60) |
| 16 | Hybrid default, RRF K=60, fail-open with truthful metadata | WORKING | query.rs:6929; lib.rs:29186, 33509 |
| 17 | `--mode semantic` fails closed, never hash-substitutes | WORKING | lib.rs:29343; embedder_registry.rs:271 |
| 18 | Two-tier progressive (fast 256-d → quality 384-d, refine in place) | STUB / DEAD_CODE | vector_index.rs:225 `owner_backed_progressive_reader:false`; query.rs:5106 always false; two_tier_search.rs zero callers; hash is 384-d not 256-d; `--two-tier` → Single (lib.rs:29388) |
| 19 | HNSW ANN + `hnsw_ready` | PARTIAL / WRONG | sidecar only via `index --build-hnsw`/`models build-hnsw`; query needs `--approximate`; `hnsw_ready = is_file()` (asset_state.rs:935); bead wfm4e P0 open |
| 20 | Blue-green semantic generation + manifest + one readiness classifier | STUB | semantic_manifest.rs has no production writer; sole reader (model_manager.rs:319) fails closed if `current.json` exists; four parallel readiness enums; ds7uy P0 epic accurate, 0 commits since Aug 1 |
| 21 | Native MiniLM + multilingual, opt-in install, checksums, `--from-file` | WORKING / UNPROVEN | code real (model_download.rs:1194, lib.rs:116141); zero repo tests load a real model; fixtures are int8 ONNX (wrong format) |
| 22 | Warm daemon, per-dir socket, pinned key, v2 attestation, index timer | WORKING | protocol.rs:21, mod.rs:139-385, core.rs:344 |
| 23 | Cross-encoder reranker | WORKING | fastembed_reranker.rs:14; lib.rs:29684 |
| 24 | Query language (AND/OR/NOT, phrases, wildcards, time input) | WORKING (not re-audited this pass) | tests/search_*.rs |
| 25 | Quill scales on append-only archives (#441) | REGRESSED / NO_BEAD | compaction only in watch closure (mod.rs:16586); `query_fuel_budget` default 10M, no override; hybrid hard-fails via `?` (query.rs:6907) |

### 2.3 Robot / agent surfaces

| # | Goal | Status | Evidence |
|---|---|---|---|
| 26 | 23-layer forgiving syntax with teaching notes | WORKING / PARTIAL | 36 README rows verified on binary; teaching note gated `!is_robot_mode` (lib.rs:6730) so `--json` callers never see it; subcommand typos ARE corrected (README says not) |
| 27 | triage/capabilities/introspect/api-version/robot-docs/selftest | WORKING | api-version fields differ from README sample; installed 0.6.26 `selftest` is a search for the word "selftest" (fixed on main) |
| 28 | Exit codes 0–24, ~50 kebab kinds, envelope | PARTIAL | 93 kinds (4 snake_case legacy); exit 70 bypasses envelope (mod.rs:3475); code 8 exists only for sources sync |
| 29 | `--timeout` → partial results, exit 8 | WRONG_DOC | always exit 10 `timeout`; `output_search_budget_partial` returns 10 (lib.rs:28299) |
| 30 | `--robot-meta` (cache_hit, next_cursor, index_freshness…), `--fields`, `--aggregate`, `--cursor`, `--highlight`, `--sessions-from`, `--explain`, `--dry-run`, `--trace-file` | WORKING | no `cache_hit` key (has `cache_stats`); `<mark>` only in pages/fts.rs, not html_export; unknown `--source` → silent empty |
| 31 | Per-hit `trust` block with beads/git provenance join | WORKING | trust_correlation.rs:433, 585 |
| 32 | `cass pack` answer packs (budgets, freshness, privacy, warnings) | WORKING | pack_planner.rs:1503-1659 |
| 33 | `swarm status/work-packet/lint` composing Beads + Agent Mail + git + rch | STUB | swarm_status.rs:1-6 "avoids live provider calls"; only `FixtureSwarmSourceAdapter`; live path returns `live-provider-unimplemented`; only `dependency-drift` is live |
| 34 | Golden-pinned contract surfaces | PARTIAL | 50 tracked goldens + 22 stale `.actual`; no triage golden; pack goldens are error envelopes only |
| 35 | `health` < 50 ms, bounded, read-only | REGRESSED | 202 ms here but `open_franken_readonly_storage_with_timeout` can do read-write open + `wal_checkpoint(TRUNCATE)` (sqlite.rs:1073-1107); only `status` got the bounded probe; bead k2k20 half-fixed |

### 2.4 TUI

| # | Goal | Status | Evidence |
|---|---|---|---|
| 36 | Three-pane, live footer with sparkline | PARTIAL | no sparkline in indexing footer (only in stats bar) |
| 37 | Keyboard reference | WRONG_DOC (~60% correct) | `A`→Alt+A, `Shift+D`→Ctrl+D, density rows 2/5/6 not 3/5/8, saved views Ctrl+1-9/Shift+1-9, no fullscreen, no `n`/`N` find, `?`/`y`/`o`/`c`/`1-9` type into query; in-app help also stale (app.rs:12450) |
| 38 | 19 themes, WCAG contrast, adaptive borders, role styling | WORKING | theme.rs:768-860; style_system.rs:860-962 (report fn tests-only) |
| 39 | 7-view analytics dashboard, KPI tiles, drill-down | WORKING | app.rs:650-665, 20354 |
| 40 | Bookmarks (`bookmarks.db`, notes/tags/export) | DEAD_CODE | bookmarks.rs full API, zero callers from ui/ or CLI |
| 41 | Saved views, toasts, command palette, inline mode, macro/asciicast | WORKING | palette has 26 actions (README lists 15) |
| 42 | TUI stays responsive on large archives (#395) | PARTIAL / NO_BEAD | initial browse bounded in v0.7.1; `AnalyticsLoadRequested` runs in-process full rollup rebuild (app.rs:20428); `load_semantic_context` synchronous before first frame (app.rs:23397) |
| 43 | Snapshot baselines | STALE | 15 of 36 last blessed 2026-02-06 |

### 2.5 Peripheral

| # | Goal | Status | Evidence |
|---|---|---|---|
| 44 | `sources setup` wizard incl. install chain and final sync | PARTIAL / STUB | setup.rs:1145-1159 "We don't actually run sync here"; install.rs:618-681 no fall-through; probe cache dead |
| 45 | rsync flags as documented; SFTP fallback; additive-only | WORKING (flags WRONG_DOC) | sync.rs:152-165 |
| 46 | HTML export encrypted, `--password`, Tailwind+Prism | PARTIAL | no `--password` (only stdin); Tailwind never loaded; JSON shape differs; exit 9 undocumented; bead 34irx done-unclosed |
| 47 | Pages encrypted static-site export | WORKING / UNDOCUMENTED | 45K lines, real GH/Cloudflare deploy, 742+249 tests; zero README mentions; `cass pages key *` in docs/RECOVERY.md does not exist |
| 48 | Doctor v2 (check/repair/backups/cleanup/support-bundle), never deletes | WORKING | `doctor check --json` 1.2 s on 10.3 GB; verb tree via argv rewriter (lib.rs:5313-5830) |
| 49 | Analytics rollups, `analytics rebuild --days`, `incidents` miner | WORKING | sqlite.rs:6385-6467; validate.rs; incident_redaction.rs |
| 50 | Installer glibc 2.38 gate with source fallback | NOT_STARTED | install.sh has no glibc check |
| 51 | Self-update with backup + rollback | PARTIAL | update_check.rs:351-500 execs installer; no backup |
| 52 | Homebrew bottles | WRONG_DOC | tap has no `bottle` block; prebuilt tarballs |
| 53 | CI pipeline "runs on every PR and push" with coverage/bench/fuzz/browser | NOT OPERATING / NO_BEAD | all workflows disabled_manually |
| 54 | UBS blocking pre-merge gate (AGENTS.md) | NOT OPERATING | ci.yml disabled; local `ubs` v5.3.13 vs pinned "latest" |

---

## 3. What is verified working right now

- End-to-end discovery → persist → lexical index → robot search on a real 10 GB archive (this host, installed 0.6.26): 3/3 queries returned correct hits with truthful `fallback_tier:"lexical"` metadata.
- Atomic lexical publish with retention and crash recovery; schema-hash self-heal; stale-on-read background refresh; OS scheduler; watch mode.
- Doctor v2 read-only surfaces are bounded and fast (`doctor check` 1.2 s on 10.3 GB).
- Forgiving CLI syntax (36 documented rows verified), triage/capabilities/introspect, cursors, aggregations, packs, trust blocks with real beads/git joins.
- Daemon attestation (challenge + HMAC, pinned key, per-dir socket).
- Hash tier, RRF fusion, reranker, hybrid fail-open, semantic fail-closed.
- TUI themes, analytics views, palette, saved views, toasts, inline/macro/asciicast.
- Raw mirror chunking and audited prune; index-time redaction; FTS repair streak escalation.
- Pages export + deploy (hidden); remote sources add/list/sync/mappings/doctor; analytics rebuild/validate/incidents.
- Packaging: installers, release binaries for 5 targets, crates.io, brew tap, scoop bucket.
- `unsafe` is contained (12 FFI/env sites); `SendFrankenConnection` is gone.

---

## 4. Gap analysis by category

### 4.1 Vision gaps (documented, no code path)
- Two-tier progressive refinement (README 226-234, 284-303).
- Bookmarks (README §Bookmark System).
- Swarm live composition (README §Swarm Operations Workflow).
- `--timeout` partial results / exit 8.
- Installer glibc gate + source fallback; self-update backup.
- `sources setup` final sync; install method fall-through.
- HTML export `--password`, Tailwind, `<mark>`.
- Pages key-management CLI (docs/RECOVERY.md).

### 4.2 Implementation gaps (bead exists, code incomplete)
- ds7uy tree (manifest writer/reader, crash-safe reindex, one readiness classifier, ensure-ready) — dormant since July 30.
- wfm4e `hnsw_ready` truthfulness.
- fyepq per-source ingest ledger (GH#426).
- k2k20 bounded `health` open (status fixed, health not).
- cjugu/bet45 lexical-rebuild wedge family (3 red tests; cass-side suspects f273ccc4, 7af85c82).
- aegfi Windows Quill writer proof; 4w0ma Windows exit panic.
- gothf rustsec baseline (paste, nix, rustls-pemfile).

### 4.3 Proof gaps (code exists, no evidence)
- CI: nothing runs. No full integration-suite receipt. Coverage/bench/fuzz/browser dormant since Aug 20.
- Real-MiniLM: 0 tests load the native model; fixtures are ONNX.
- Sub-60 ms: no benchmark gate; `search_latency_e2e.rs` exists but is not run.
- e2e SSH sources: real sshd Docker tests, all 9 ignored.
- Snapshot baselines: 15/36 from February.
- Windows: no receipt for v0.7.1 Quill writer (#429).

### 4.4 Performance gaps
- Robot per-call overhead 1.1 s (integrity preflight + open) dominates a 86 ms search.
- #441 monotone segment growth; query fuel exhaustion for 6+ word queries.
- FTS5 write cost O(rows-so-far) per batch (#379) with no governor in sqlite.rs.
- Migration V18–V20 unbounded single transactions; engine backup doubles disk.
- TUI: in-process analytics rebuild and synchronous semantic load on startup path (#395).

### 4.5 Integration gaps
- #439 post-publish phase-0 work has no progress ticks → false exit 70.
- #440 interrupted force-rebuild leaves cursor behind published Quill authority → exit 9.
- `cass status` doc-count fallback still opens a Quill dir with the Tantivy reader (lib.rs:21282).
- `preferred_backend:"fastembed"` and `OnnxEmbedderConfig` survive in contracts after ONNX removal.

### 4.6 Design gaps
- lib.rs holds the business logic of doctor (3,327-line `run_doctor_impl`), search rendering, index, export, pack, status, schemas; 32 fns > 300 lines. Tests in lib.rs string-scan lib.rs source (doctor.rs:1543).
- ~3.7K lines of dead staged-shard code + 674 lines federated helpers + 33 `CASS_TANTIVY_*` vars (~10 no-ops) behind `staged_shard_plan = None` (mod.rs:22305).
- Four semantic readiness enums; manifest scaffolding unreachable.
- Doctor asset taxonomy blind to engine `pre-migration-bak` / `.fsqlite-migration-state` (permanent 10 GB here).
- No `#![deny(unsafe_code)]` fence; only a string-grep test guards Send/Sync regressions.

---

## 5. Bead coverage cross-check

### 5.1 NO_BEAD gaps (worst class)
| Gap | Evidence |
|---|---|
| #441 Quill segment growth / fuel exhaustion / hybrid hard-fail | filed today; 0 beads match |
| #439 v0.7.1 index --full false exit 70 after healthy publish | filed today; 0 beads |
| #440 resume cursor behind published authority → exit 9 | filed today; 0 beads |
| #395 TUI startup hang on large archive (residual: analytics rebuild, semantic load) | open since Aug 12; 0 beads |
| #391 recurring btree corruption (rowid-out-of-order) | open since Aug 10; 0 beads, 0 code refs |
| #423 Freebuff connector | 0 beads |
| CI/workflows disabled; no automated gate | 0 beads (uojcg.11 epic is about proof gates but has no CI item) |
| lib.rs decomposition | 0 beads |
| Dead Tantivy/staged-shard code removal | 0 beads |
| README/AGENTS.md truth pass (schema v20, keys, dedup, timeouts, exports, swarm, Pages, 14 subcommands) | 0 open beads (3e3qg.7 was closed and flagged false-closed by the May compliance audit) |
| Bookmarks wiring or removal | 0 beads |
| Robot per-call overhead / preflight cost | 0 beads (k2k20 is about hangs) |
| Teaching notes suppressed in robot mode | 0 beads |
| `--timeout` exit-8 contract | 0 beads |
| `health` bounded open (half of k2k20) | k2k20 open but its "fixed-at-head" comment is wrong for `health` |
| Snapshot re-bless | 0 beads |
| Installer glibc gate; self-update rollback; setup final sync | 0 beads |

### 5.2 Ledger hygiene problems
- **Done-but-unclosed in_progress**: rhmbf (P0), lukne (P1), 34irx (P1), zqre2 (P2), and ~12 Pages/secret-scan beads (45jxv, 1hg2q, 4ydds, 7y2jt, c8gx1, cc7pi, h3ibc, kjdbv, z9sg6, yjjsg, h0rss, jfcgi, …).
- **Obsolete open**: hvzel, wssow, 1ixp7 (crates.io publish — done at 0.7.0 on 2026-08-25).
- **Dormant P0**: ds7uy epic + 9 children, 0 commits since Aug 1.
- **Stale claims**: 21 in_progress beads untouched >30 days; fleet-resilience epics (uojcg.*) in_progress since June with 78/95 children closed.
- **Misattributed**: bet45 wedge blamed on fsqlite 0.3.13; git evidence points at f273ccc4 / 7af85c82.
- **Tooling**: `br show` fails with DB/JSONL divergence; `br doctor` = degraded (stale merge anchor, 2 `br` binaries on PATH).

### 5.3 Would completing all open + in-progress beads close the gap?
**No.** They would close: the semantic architecture (if ds7uy is actually executed), Windows proof, rustsec baseline, engine-blocked memory items (partially), Pages/secret-scan hardening, ingest ledger, GH#413/#422 acceptance. They would **not** touch anything in §5.1, and several of them (ds7uy) are blocked on frankensearch primitives rather than cass code.

---

## 6. What is blocking

1. **Engine boundary.** cass is downstream of two young engines (frankensqlite 0.3.13, frankensearch/Quill 0.4.2) and absorbs their scale failure modes: FTS5 memory hydration, WAL open-path cost, btree corruption, Quill compaction policy and fuel budget. Several cass-side mitigations are "refuse" rather than "fix" (sqlite.rs:5607).
2. **No ratchet.** With CI disabled and the lib suite red, nothing prevents regression; the only receipts are agent-run rch commands, and rch itself refuses jobs under fleet pressure (`RCH_REQUIRE_REMOTE=1`, exit 103 ×3 today).
3. **Monolith.** 119K-line lib.rs and 58K-line indexer make every review, decomposition, and dead-code removal expensive; the "no file proliferation" rule has over-corrected into "no files at all."
4. **Process drift.** Velocity (396 commits/week) is pointed at beads, not at users: three P0-class issues filed today have no bead; the tracker has ~14 finished-but-open items and 3 obsolete ones; the frozen README-truth bead was false-closed in May.
5. **Semantic is a two-repo program with no owner.** ds7uy needs frankensearch primitives and a real-model test lane; it has had no activity for five weeks while the README continues to advertise it.

---

## 7. Bridge plan (Phase 2, comprehensive — revised in place 2026-09-01 evening)

This section is the Phase 2 deliverable of the reality-check flow: a plan
detailed enough to close every gap in §2–§6 *properly* — harmonized with the
existing architecture, with the highest reliability, performance, and
robustness — and detailed enough that Phase 3a beads can be generated from it
without consulting any other document. It supersedes the earlier summary that
lived here; the progress log in §7b records what already landed today.

### 7.0 How to read this plan

- **Task ids** are `WS-<letter>.<n>`. Each task states: *Change* (what, and
  where in the code), *Acceptance* (a positive observable plus a planted
  negative, so a green cannot be a zero-run green), *Proof* (the unit and
  end-to-end tests and the logging/receipt that demonstrates it), *Depends on*,
  *Size* (S ≤ ½ day, M ≤ 2 days, L ≤ 1 week, XL = program), and *Owner class*
  (`cass` = pure cass change; `engine` = needs frankensqlite/frankensearch;
  `owner` = a decision only the project owner can make).
- **Status** reflects HEAD `73c471cc` plus the working tree at the time of
  writing: `DONE` (landed today, see §7b), `PARTIAL`, `OPEN`.
- Every task serves one or more **end states** from §7.1; the mapping is the
  justification, and a task that serves none should be dropped, not done.
- **Standards that apply to every task** (§7.4): no mocks presented as live
  proof; tests carry a positive observable and a planted negative; every
  long-running path emits a structured receipt (tracing event or JSON) that
  the test asserts on; nothing closes on "compiles" alone; a closure cites the
  exact commit and the command whose output proves the acceptance.

### 7.1 Definition of done — the vision as testable end states

| End state | Testable statement | Today |
|---|---|---|
| **ES1 Robot latency** | A warm `cass search --robot` on a 10 GB, 500k-message archive answers in ≤ 300 ms wall (engine ≤ 60 ms) with no archive mutation. | 8.7 s in v0.7.1; fix landed, unmeasured |
| **ES2 Index reliability at scale** | `cass index --full`, `index` (incremental, interrupted or not), and `--watch` complete on a 15k-conversation / 2M-message / 10 GB archive without exit 70, exit 9, OOM, or wedge, and every run ends with a truncated WAL and a consolidated lexical generation. | #439/#440/#441/#413/#422 open |
| **ES3 Truthful search modes** | Hybrid = lexical + semantic refinement when the model is installed, proven by a real-model test; without a model, lexical fail-open with `_meta` telling the truth; no advertised mode is unreachable at runtime. | Progressive/two-tier unreachable; no real-model test |
| **ES4 Observation surfaces never pay** | `health`, `status`, `diag`, `doctor check`, `search --no-maintenance` never mutate the archive and answer within a hard bound on any archive size. | health fixed today; others bounded |
| **ES5 Docs are executable truth** | Every README claim maps to a golden, a test, or a validator; a docs validator runs in the gate. | README corrected; validator missing |
| **ES6 A gate exists** | Every push to main runs fmt, clippy `-D warnings`, `cargo test --lib`, goldens, UBS; red main blocks bead closure. | All workflows disabled; agent-run only |
| **ES7 No dead subsystems** | Zero modules with no production caller; zero env vars that are no-ops; no Tantivy-only code behind a hardcoded `None`. | ~4.4K dead lines, 5 dead modules |
| **ES8 TUI at scale** | First frame ≤ 2 s and interactive on the 2M-message archive; no in-process multi-GB work on the effects thread. | Two hazards fixed today, unproven |
| **ES9 Structure supports change** | `src/lib.rs` < 30K lines; no function > 300 lines in moved code; monoliths split by surface; tests do not string-scan source. | lib.rs 119K |
| **ES10 Release discipline** | v0.7.2 ships from a green gate with reporter retests for #439/#440/#441/#395 attached and a Windows receipt. | No CI; dsr config missing |
| **ES11 Tracker equals reality** | No done-but-open beads; every GitHub issue has a bead within 24 h; stale claims re-triaged monthly. | 14 done-but-open closed today; 12 remain |
| **ES12 Safety fence** | `deny(unsafe_code)` outside tests; no raw cross-thread engine handles; secret redaction proven on every export surface. | Fence landed today |

### 7.2 Workstreams

#### WS-A — Restore the quality gate (ES5, ES6)

**A.1 Re-enable the CI workflows (owner + cass).** *Change:* `gh workflow enable` for `CI`, `Fresh Clone Build`, `Coverage`; trim `ci.yml` to the four blocking jobs (fmt, clippy `-D warnings`, `cargo test --lib`, goldens) plus `ubs-changed-files`, moving fuzz/bench/browser to `workflow_dispatch` and nightly schedules so a push never queues five workflows. If Actions minutes are the reason they were disabled (owner decision), add a self-hosted runner label backed by one rch worker and point only the blocking jobs at it. *Acceptance:* a green run on main within 24 h of the change; a deliberately red PR (planted: a test that panics) is blocked. *Proof:* the run URL recorded in bead notes; the planted-red PR's failed check. *Depends on:* owner decision on Actions. *Size:* M. *Status:* OPEN.

**A.2 Batched agent gate until A.1 lands (cass).** *Change:* add `scripts/gate.sh` that runs exactly what CI will run through ONE `rch exec --job --result-dir tests/golden` admission (fmt check, clippy, `cargo test --lib`, targeted integration tests, goldens verify) and prints one receipt line per stage (`STAGE=<name> EXIT=<code>`), so a fleet admission is never spent on `cargo check` alone. *Acceptance:* one invocation produces all receipts; a planted clippy warning yields `STAGE=clippy EXIT=101`. *Proof:* the script's own receipt output committed under `docs/artifacts/gate-receipts/<date>.txt` for the first run only (retire once A.1 is live). *Size:* S. *Status:* OPEN (the pattern was used ad hoc today).

**A.3 Lib suite green (cass).** *Change:* root-cause the bet45 wedge family (`lexical_rebuild_packet_producer_builds_lookup_and_source_context_internally`, `…respects_planned_shard_boundaries`, `rebuild_tantivy_from_db_promotes_pipeline_budgets_after_first_commit`) starting from the two cass commits git evidence points at — f273ccc4 (content-bounded page limit forcing 1-conversation pages under tiny test budgets) and 7af85c82 (strict read-only page-prep open that returns errors instead of self-healing, which parks the producer at `result_rx.recv()` if a worker errors before `wait_for_turn`). Fix the producer's error-before-turn path to close the ordering so a worker failure cannot park the producer; make the shard-boundary test's budget realistic or make `finishes_planned_shard` reflect the content bound. *Acceptance:* `cargo test --lib` reports 0 failed, 0 newly ignored; the wedge test finishes in < 10 s; a planted negative (a worker that errors before taking its turn) fails the run with a typed error instead of hanging. *Proof:* full lib-suite receipt in the bead; `CASS_PIPELINE_TRACE=1` trace attached showing the error path unparking the producer. *Size:* M–L. *Status:* OPEN.

**A.4 Full integration receipt (cass).** *Change:* run `cargo test --all-targets` through A.2, triage every failure into a bead with its panic text, un-ignore nothing. *Acceptance:* one receipt with pass/fail/ignored counts per test binary. *Size:* S (plus follow-up beads). *Status:* OPEN.

**A.5 Unsafe fence (cass).** *Status:* DONE today (`#![cfg_attr(not(test), deny(unsafe_code))]` in lib.rs and main.rs, 14 scoped allows with SAFETY comments, fence-presence test). Remaining: none.

**A.6 Golden hygiene and isolation (cass).** *Change:* (a) add `triage.json.golden` and `triage_shape.json.golden`; (b) remove the 22 untracked `tests/golden/robot/*.actual` files (owner permission required by AGENTS.md rule 1); (c) move `tests/golden/swarm_status/` under `tests/golden/robot/swarm/`; (d) make the golden harness refuse to regenerate unless `CASS_DATA_DIR` points inside the test's temp dir — today's remote regeneration leaked the worker's real archive into `search_robot` and `stats_full_payload` goldens (§7b), which is exactly the failure mode this guard prevents; (e) normalize trailing newlines in the writer so regen never produces newline-only diffs. *Acceptance:* `UPDATE_GOLDENS=1` on a host with a populated default data dir produces goldens identical to a clean host (planted: run with `CASS_DATA_DIR` unset → the harness errors instead of writing). *Proof:* two regen runs diffed. *Size:* S. *Status:* OPEN.

**A.7 Snapshot baselines (cass).** *Change:* re-bless the 15 February snapshot baselines following `docs/planning/TESTING.md` review checklist; add a `snapshot_age_days` assertion that fails when a baseline is older than 120 days without a `# reviewed <date>` marker. *Acceptance:* all 36 baselines reviewed and dated. *Size:* M. *Status:* OPEN.

**A.8 Performance ratchet (cass).** *Change:* wire `benches/search_latency_e2e.rs` into A.2/A.1 as a ratchet with two bounds on the bench corpus: engine p95 ≤ 60 ms and one-shot `cass search --robot` wall ≤ 300 ms; store results in `.bench-history/` per the gauntlet keep-gate rules. *Acceptance:* a planted regression (sleep 400 ms in the read path) fails the ratchet. *Depends on:* B.8. *Size:* M. *Status:* OPEN.

**A.9 Docs validator (cass).** *Change:* `scripts/validate_docs.sh` (extend the existing script) checks: every key binding named in README's keyboard tables exists in `impl From<Event> for CassMsg`; every CLI flag and subcommand in README exists in `cass introspect --json`; every env var in the README table exists in `cass robot-docs env`; every `cass <verb>` in AGENTS.md examples parses under `--dry-run` where available. *Acceptance:* the validator is red on a planted fake key (`Ctrl+Shift+Q`) and green on HEAD. *Proof:* run in A.2. *Size:* M. *Status:* OPEN.

**A.10 fmt drift (cass).** *Change:* run `cargo fmt` once over the pre-existing drift (raw_mirror.rs: 43 diffs at HEAD; mod.rs json_heap_bytes and friends) in its own commit so the fmt gate can be blocking. *Acceptance:* `cargo fmt --check` exit 0. *Size:* S. *Status:* OPEN.

#### WS-B — Large-archive reliability (ES1, ES2, ES4)

**B.1 #441 segment growth and query fuel (cass; engine follow-up).** *Status:* PARTIAL. Landed today: `cass_quill_config()` at every reader/writer open (visibility-lag seals disabled so snapshots publish only on cass commits — the root cause of per-second segment explosions; fuel budget at engine default with `CASS_QUILL_QUERY_FUEL_BUDGET` as an escape hatch), `optimize_if_idle` after incremental runs and `force_merge` (concat merge) after full rebuilds, hybrid degrade on fuel exhaustion with `_meta.lexical_degrade_reason`, an actionable lexical-only hint, and tests (`post_run_maintenance_bounds_segment_growth_on_append_only_commits`, `bounded_merge_folds_session_segments_into_one`, fuel recognition). *Remaining changes:* (a) expose `lexical_segment_count` and `lexical_last_merge_at` in `status --json`/`health --json`/`doctor check` with a `segment_pressure` finding when count > 8× threshold; (b) an e2e that ingests a 600-segment fixture (built by committing 600 tiny batches with maintenance disabled), runs `cass index`, and asserts the segment count falls below threshold and an 8-word stopword query succeeds within the default fuel budget; (c) decide with the owner whether the default budget should be raised for archives that fragmented before this fix (recommendation: keep the engine default now that consolidation is automatic, revisit after reporter retest); (d) reporter retest on #441 with `cass status --json` attached. *Acceptance:* (b) passes; planted negative: with maintenance disabled the same query returns the fuel error and the hint. *Proof:* e2e receipt; `_meta` JSON. *Size:* M. 

**B.2 #439 false exit 70 after a healthy publish (cass).** *Status:* PARTIAL. Landed today (by a parallel agent, same design as planned): the post-publish FTS shadow rebuild ticks the indexer activity counter per page. *Remaining changes:* (a) every other post-publish phase-0 step must tick too — `optimize_if_idle` (now runs post-publish), daily-stats rebuild, checkpoint refresh — add a `phase0_maintenance` heartbeat wrapper that ticks before and after each step and logs `phase0_step=<name> ms=<n>`; (b) make the stall watchdog phase-aware: while `IndexingProgress.phase == 0 && published_generation_ready`, apply the finalize-class grace and require BOTH zero activity AND zero block-IO advance for the full window before aborting; (c) e2e: publish a generation on a fixture, inject `CASS_TEST_FTS_REPAIR_SLOW_MS=90000` with `CASS_INDEX_STALL_ABORT_SECS=30`, assert exit 0 and the `phase0_step` receipts; planted negative: a genuinely wedged post-publish step (test hook that parks) still aborts at the finalize threshold with `kind:"index-stalled"`. *Depends on:* F.2. *Size:* M.

**B.3 #440 resume behind published authority (cass).** *Status:* PARTIAL (visibility-lag fix removes the engine's independent publication; the reconcile gap remains). *Change:* in `reconcile_pending_lexical_commit`, read the published Quill manifest's doc identities for the staging generation and compare with the durable cursor; if the manifest is ahead, advance the cursor to the manifest's last committed conversation (ids are monotonic) instead of re-inserting; if behind, continue; never return exit 9 for a self-inflicted lag — reserve 9 for a genuinely missing/unpublishable index. Persist the reconcile decision in `.lexical-rebuild-state.json` (`reconciled_from_manifest_at`). *Acceptance:* e2e: start `index --force-rebuild` on a 300-conversation fixture, SIGTERM after the first staged MANIFEST publish, run plain `index` → exit 0, all conversations searchable, no duplicate identities; planted negative: delete the staging MANIFEST → the run rebuilds from scratch and reports it. *Proof:* e2e with `CASS_PIPELINE_TRACE=1` receipts. *Size:* M.

**B.4 Bounded observation surfaces (cass).** *Status:* PARTIAL. Landed today: `health` uses the strict bounded owner-thread probe. *Remaining:* (a) expose `db_bytes`, `wal_bytes`, `shm_present`, `probe_ms` in `health`/`status` so an oversized WAL is visible; (b) reporter-scale proof: run `health` and `status` against a 9 GB archive with a 200 MB dirty WAL and record wall time and `wal_bytes` before/after (must be unchanged); (c) `diag --json` must use the same strict probe (audit `probe_state_db` callers for any inline read opener left). *Acceptance:* all three surfaces < 1 s on the owner's archive with WAL unchanged; planted negative: a non-SQLite file surfaces as a hard open failure on every surface. *Size:* S–M.

**B.5 WAL left untruncated by non-index write paths (cass; bead z2uon).** *Change:* (a) audit every path that writes the archive outside `run_index` — engine migration on first open, `doctor --fix` repairs, `analytics rebuild`, `quarantine retry`, `forget`, `dedup`, `sources agents exclude` — and route each through the same close-with-`wal_checkpoint(TRUNCATE)` finalize used by `close_storage_after_index`; (b) add a `wal_oversized` doctor check (WAL > 64 MiB with no `index-run.lock` holder) and a `--fix` repair that runs a bounded TRUNCATE checkpoint with the usual fingerprinted receipt; (c) `cass index` refuses nothing but logs `wal_bytes_before/after` at finalize. *Acceptance:* after each mutating command on a fixture, `wal_bytes < 32 MiB`; doctor detects a seeded 200 MB WAL and the repair truncates it without changing row counts; planted negative: with a concurrent reader pinning the WAL, the repair reports `busy` truthfully and leaves the WAL. *Proof:* per-command e2e receipts. *Size:* M.

**B.6 #391 recurring btree corruption (cass; engine).** *Change:* (a) `doctor check` runs `PRAGMA quick_check` plus a rowid-monotonicity probe on `conversations` and `messages` (bounded by the existing bundle ceiling) and reports `btree_rowid_order` with the offending rowid range; (b) when the engine migration marker records `repair_orphaned_pages > 0`, doctor emits a `corruption_history` finding and offers `support-bundle`; (c) cross-link the frankensqlite bead for the writer-side cause and attach the owner's marker file as evidence. *Acceptance:* a fixture with an out-of-order rowid (constructed with a raw write) is detected; planted negative: a healthy fixture reports none. *Size:* M (cass side).

**B.7 Memory governor and migration safety (cass).** *Change:* (a) consult `responsiveness` (in-flight bytes/worker clamps) inside `stream_fts_rows_via_frankensqlite` paging and inside migrations V18–V20 (keyset pages, one transaction per page, resumable via `_schema_migrations` progress row); (b) before any engine migration that copies the DB, require free space ≥ 2× DB + WAL and refuse with `kind:"disk-space"` otherwise; (c) classify the engine's `*.pre-migration-bak*` and `.fsqlite-migration-state` in doctor's asset taxonomy with sizes and a fingerprinted cleanup plan (never automatic). *Acceptance:* a 1M-row fixture FTS rebuild stays under a 1 GB RSS cap (measured by the existing memory tests); migration on a tmpfs with < 2× free refuses; doctor lists the backup with its byte size. *Size:* M–L.

**B.8 Robot per-call overhead (cass).** *Status:* PARTIAL. Landed today: the lexical self-heal fingerprint is memoized on the archive's physical identity, removing the synchronous storage open that replayed the whole WAL. *Remaining:* (a) re-measure on the owner's archive with a main build (target: `other_ms` < 150, wall ≤ 300 ms warm); (b) audit the remaining read-path work between process start and `search_start` (state probe, checkpoint load, storage-integrity dedicated probe, semantic context load) and make each either O(1) or cached on the same identity; (c) make the strict `--no-maintenance` opener the default for the *read* half of a search, taking a maintenance-capable open only after the cheap signals say maintenance is needed. *Acceptance:* A.8 ratchet green; planted negative: touching the WAL invalidates the cache and the next search recomputes (observable via the `archive_fingerprint_cache=miss` debug event). *Size:* M.

**B.9 #395 TUI at scale (cass).** *Status:* PARTIAL. Landed today: lazy semantic loader before the first frame; analytics rollup rebuild spawned as a detached child. *Remaining:* (a) headless e2e on the 2M-message fixture measuring time-to-first-frame ≤ 2 s and RSS ≤ 1 GB at first frame; (b) audit every `Cmd::task` closure for archive-scale work (grep `FrankenStorage::open` in `src/ui/`) and bound each with the same detach-or-page rule; (c) reporter retest. *Acceptance:* (a) passes; planted negative: reintroducing an in-process rebuild in a test build trips the RSS assertion. *Size:* M.

**B.10 GH#426 per-source ingest ledger (cass; bead fyepq).** *Change:* persist a per-source observation ledger (`<data_dir>/index/.ingest-ledger.json`: source path, mtime, size, last committed conversation id, status) updated at every batch commit; on resume, skip sources whose ledger row matches and re-parse only unfinished ones; retire `refresh_ledger.rs` if it stays test-only after this lands. *Acceptance:* kill an incremental run mid-way on a 500-session fixture → the re-run reports `resumed_sources=N` and parses only the unfinished ones (counted via the ingest trace); planted negative: touching a finished source's mtime re-parses it. *Size:* L.

**B.11 #413 / #422 acceptance (cass).** *Change:* attach reporter-sized scratch proofs (the 2.1 GB fixture recipe in the bead) for the sink-starvation flush and the search-triggered refresh watchdog; un-ignore the ignore-gated `gh413_full_rebuild_drains_when_one_conversation_exceeds_the_inflight_budget` once B.12's ingest bounding lands. *Size:* M (proof runs).

**B.12 Engine-gated memory items (#379, #345, #329, #349, #320) (engine; cass mitigations).** *Change:* for each, keep the cass-side bound (batch caps, refusal with actionable hint, deferred repair) and cross-link the frankensqlite bead; add a `doctor check` finding `engine_bound_pending` listing which of these apply to the archive (size class, FTS shadow size) so users know why a repair is deferred. *Acceptance:* the finding appears on the owner's 10 GB archive and not on a 50 MB fixture. *Size:* S (cass side).

**B.13 Windows receipts (#429, #406) (cass; dsr).** *Change:* register the cass dsr config (bead yviq2), build the windows/amd64 candidate, run `selftest`, `index --full` on a small corpus, and a cold query on Windows 11; fix the on-exit "threads should not terminate unexpectedly" panic (#406) by joining the sync-bridge runtime before exit. *Acceptance:* the receipt in the bead; planted negative: the pre-fix binary reproduces #406. *Size:* M.

#### WS-C — Semantic: decide, then either ship it or retire it (ES3)

**C.1 Decision (owner).** Documentation now tells the truth (progressive/two-tier inactive, ANN opt-in). Decide between (a) funding the semantic program below as one owned effort with a frankensearch counterpart, or (b) retiring the unreachable code (two_tier_search.rs, writer-less manifest, four readiness enums) and keeping hybrid = lexical + single-tier refinement. Recommendation: (b) for structure now (E.4), (a) as a scheduled program only if a real-model lane (C.2) exists first. *Size:* owner decision.

**C.2 Real-model test lane (cass).** *Change:* a gate job that downloads the safetensors `all-MiniLM-L6-v2` once into a cached path, runs the 8 ignored embedder/reranker tests, and a restart/degrade/recover e2e: install → backfill → query (semantic hits present) → delete the model → query (lexical fail-open with `fallback_mode=lexical`) → reinstall → query. Replace the int8 ONNX fixtures under `tests/fixtures/models/` with the safetensors files or a documented download step. *Acceptance:* the lane is green on main; planted negative: corrupt `model.safetensors` → `models verify` fails and search reports `semantic_unavailable`. *Size:* M.

**C.3 `hnsw_ready` truth and ANN by default (cass; bead wfm4e).** *Change:* `hnsw_ready` is computed by opening the sidecar through the native admission path (owner identity match, dimension match, checksum); backfill builds the sidecar when the quality tier completes; hybrid uses ANN whenever admitted, with `_meta.ann_admitted=true|false` and the reason. *Acceptance:* a stale sidecar from another embedder reports `hnsw_ready=false` with reason; planted negative: a truncated `.chsw` is refused, not served. *Size:* M.

**C.4 Generation manifest writer + reader together (cass; beads ds7uy.1/.3).** *Change:* land the immutable generation manifest writer and the manifest-only reader in one change with an atomic `current.json` pointer; never ship the reader's fail-closed check without the writer. *Acceptance:* backfill publishes a generation, `models status` shows it, a query reads only that generation; planted negative: a half-written manifest is ignored and the previous pointer serves. *Size:* L.

**C.5 One readiness state machine (cass; bead ds7uy.4).** *Change:* collapse `SemanticAvailability`, `SemanticReadinessState`, `SemanticReadinessReason`, and `TierReadiness` into one classifier consumed by health, status, doctor, and the query path. *Acceptance:* a property test enumerating on-disk states (model absent/present, vectors absent/partial/complete, ANN absent/stale/ready) produces identical readiness from all four surfaces. *Size:* M.

**C.6 Contract leaks (cass).** *Change:* rename `preferred_backend:"fastembed"` → `"native"` behind a one-release alias in goldens, remove `OnnxEmbedderConfig`, and delete the `semantic` feature remnants. *Size:* S.

**C.7 Daemon lifecycle e2e (cass).** *Change:* e2e that spawns the daemon, verifies attestation, runs a search through it, sets `CASS_DAEMON_INDEX_INTERVAL_SECS=1` and asserts one detached index child is spawned, then idles it out. *Size:* S.

#### WS-D — Docs as executable truth (ES5)

**D.1 README/AGENTS correction pass.** *Status:* DONE today (15 audited items, with code citations, plus the behaviors landed today). **D.2 Hidden subcommands documented.** *Status:* DONE today. **D.3 AGENTS connector table and CI reality.** *Status:* DONE today.

**D.4 Remaining doc truth items (cass).** *Change:* (a) fix the shadowed `Alt+W` swarm-cockpit key in `app.rs` (reorder the `alt && !shift` workspace-filter arm) and document it, or drop the key; (b) align the in-app help strip and `capabilities.mistake_recoveries` text with code (Alt+N/Alt+I; typo correction statement); (c) `docs/RECOVERY.md`: replace the nonexistent `cass pages key …` commands with the real recovery flow until G.4 lands; (d) a CHANGELOG entry for everything landed today under a `[Unreleased]` heading. *Acceptance:* A.9 validator green. *Size:* S.

**D.5 Robot teaching notes.** *Status:* DONE today.

#### WS-E — Dead code and structure (ES7, ES9)

**E.1 Remove the Tantivy-only staged-shard pipeline (cass).** *Change:* delete `rebuild_tantivy_from_db_via_staged_shards`, the shard builder/merge workers and controllers, `plan_lexical_rebuild_shards_from_*`, `finalize_staged_lexical_rebuild_publish_artifact*`, `validate_*shard*`, the staged-shard settings and ~10 no-op `CASS_TANTIVY_*` env vars, the 674 lines of federated helpers in `tantivy.rs`, and the 31 "Tantivy-only" ignored tests; keep `lexical_tantivy` only for the differential oracle. Also remove the `cass status` doc-count fallback that opens a Quill directory with the Tantivy reader. *Acceptance:* `cargo test --lib` green; `rg 'CASS_TANTIVY_STAGED|staged_shard_plan'` empty; the lib suite ignore count drops by 31. *Size:* L (mechanical but large; do in three commits: settings/env, workers/controllers, builder).

**E.2 Vocabulary rename (cass).** *Change:* `rebuild_tantivy_from_db*` → `rebuild_lexical_from_db*`, user-facing "Tantivy lexical index completed" → "lexical index completed", `CASS_TANTIVY_*` → `CASS_LEXICAL_*` with one-release aliases and a deprecation warning. *Size:* M.

**E.3 Bookmarks in the TUI (cass).** *Status:* CLI DONE today. *Change:* a `b` key in the results pane and a palette action that call `BookmarkStore::add` for the selected hit, plus a `Bookmarks` filter chip. *Acceptance:* headless TUI test bookmarks a hit and `cass bookmarks list --json` shows it. *Size:* S.

**E.4 Retire unreachable semantic scaffolding if C.1 = (b) (cass).** *Change:* delete `two_tier_search.rs`, the owner-backed progressive lanes gated by hardcoded `false`, `ProbeCache`, and the vacuous `backtrace` feature. *Size:* M.

**E.5 `lib.rs` decomposition (cass).** *Change:* mechanical, behavior-preserving moves guarded by goldens and the lib suite, one commit per module: `run_doctor_impl` + doctor helpers → `src/doctor/cli.rs`; `run_cli_search`/`output_robot_results`/`execute_search_operation` → `src/search/cli.rs`; `run_index_with_data` → `src/indexer/cli.rs`; `run_export_html` → `src/html_export/cli.rs`; `run_cli_pack` → `src/search/pack_cli.rs`; `run_status`/`run_health`/`state_meta_json*` → `src/status/mod.rs`; `build_response_schemas` → `src/introspect.rs`; `normalize_args` → `src/cli_normalize.rs`. Target lib.rs < 30K lines; no moved function > 300 lines (split by extracting the match arms into named functions during the move). *Acceptance:* every golden unchanged; `cass introspect --json` byte-identical before/after each move. *Size:* XL (rolling).

**E.6 `indexer/mod.rs` and `ui/app.rs` splits (cass).** *Change:* indexer → `pipeline.rs` (producer/page-prep/sink), `publish.rs` (atomic swap, retention, recovery), `watch.rs`, `finalize.rs`; app.rs → per-surface update modules (`search`, `detail`, `analytics`, `sources`, `swarm`) with the key map in `keymap.rs`. *Acceptance:* snapshot tests and the lib suite unchanged. *Size:* XL (rolling, after E.1).

**E.7 Replace source-scanning tests (cass).** *Change:* `doctor.rs` and `lib.rs` tests that `include_str!` the crate's own source and grep it become registry tests over the real dispatch tables. *Size:* S.

#### WS-F — Robot contract (ES3, ES5)

**F.1 `--timeout` semantics.** *Status:* DONE as documentation (exit 0 budget envelope is deliberate). *Optional change:* add `--timeout-exit-code 8` for agents that want a non-zero signal; not recommended unless requested.

**F.2 Exit 70 through the envelope (cass).** *Change:* the stall abort emits `{error:{code:70, kind:"index-stalled", …}}` on stderr before exiting, with `phase`, `stall_elapsed_ms`, and the last progress fields. *Acceptance:* the B.2 planted-negative e2e parses the envelope. *Size:* S.

**F.3 Swarm live adapters (cass).** *Change:* implement the adapter trait for live data — beads from `.beads/issues.jsonl`, Agent Mail from the local archive DB or the HTTP `/mcp` endpoint when reachable, rch from `rch status --json`, git from the existing helpers — and make fixtures test-only; capabilities must say `live` when live. *Acceptance:* `swarm status --json` on this repo reports real bead counts equal to `br list`; planted negative: an unreachable Agent Mail server yields `agent_mail.status="unreachable"`, not fixture data. *Size:* L.

**F.4 Small contract holes (cass).** *Change:* add `_meta.cache_hit`; error (exit 2, `unknown-source`) on an unknown `--source`; `<mark>` wrapping in HTML export highlight; `has_json_output=true` for `bookmarks` in capabilities (subcommand-level flags must be reflected); unify the 4 snake_case error kinds to kebab-case with a one-release alias. *Acceptance:* goldens updated by intent; planted negative for `--source bogus`. *Size:* S.

#### WS-G — Peripheral correctness (ES2, ES5)

**G.1 `sources setup` (cass).** *Change:* run the final sync unless `--skip-sync`; make `install()` fall through binstall → prebuilt → cargo install → bootstrap on failure, recording each attempt in the setup state. *Acceptance:* the Docker sshd e2e completes setup + sync; planted negative: a host without cargo falls through to the prebuilt path. *Size:* M.

**G.2 Installer and self-update (cass).** *Change:* glibc ≥ 2.38 probe in `install.sh` with automatic `--from-source` fallback; self-update backs up the current binary to `<dest>.bak`, verifies the new one with `selftest`, and restores on failure. *Acceptance:* `install-test.yml` matrix includes an Ubuntu 22.04 job that ends with a working `cass --version`; planted negative: a corrupted download restores the backup. *Size:* M.

**G.3 HTML export (cass).** *Change:* decide Tailwind: either load it as documented with an `onerror` fallback or keep the current inline CSS and say so (docs already say so — keep); add `<mark>` highlighting; keep argv passwords rejected. *Size:* S.

**G.4 Pages key management (cass).** *Change:* implement `cass pages key list|add|revoke|rotate` on top of `key_management.rs` (3,915 lines, zero callers) with the LUKS-style slot semantics the spec describes, or delete the module and the doc section if the owner prefers; fix the bogus `--output` flag in `lighthouse.yml`. *Acceptance:* a rotated key opens the bundle; the revoked key does not (planted negative). *Size:* M.

**G.5 SSH e2e lane (cass).** *Change:* run the 9 ignored sshd Docker tests in a Docker-capable gate lane weekly. *Size:* S.

#### WS-H — Tracker and swarm process (ES11)

**H.1 Done-but-open beads.** *Status:* 7 closed today with commit-level evidence. *Remaining:* verify the 12 Pages/secret-scan beads (45jxv, 1hg2q, 4ydds, 7y2jt, c8gx1, cc7pi, h3ibc, kjdbv, z9sg6, yjjsg, h0rss, jfcgi) one by one against code and close each with the landing commit, or reopen with the missing acceptance condition named.

**H.2 Obsolete beads.** *Status:* DONE (crates.io trio).

**H.3 Beads for the no-bead gaps.** *Status:* 9 filed today (46nwq, 3yo55, 1xdho, 69vpt, nsleh, jrl45, xhw6y, jhkpq, z2uon). *Remaining:* file beads for A.1–A.10, B.3, B.5–B.7, B.10, B.12, C.2–C.7, D.4, E.1–E.7, F.2–F.4, G.1–G.5, H.4–H.8 when Phase 3a runs.

**H.4 Stale in-progress re-triage.** *Change:* the 21 in-progress beads untouched > 30 days get a dated note from a current owner or return to open; the fleet-resilience epics (78/95 children closed) get closed or re-scoped.

**H.5 bet45 narrative.** *Change:* correct the root-cause note to the cass-side suspects (see A.3).

**H.6 `br` workspace health.** *Change:* run `br doctor --repair` on a preserved copy (owner's call), refresh the stale `beads.base.jsonl` merge anchor, remove the duplicate `br` from PATH.

**H.7 Issue → bead rule.** *Change:* AGENTS.md rule: a GitHub issue gets a `gh<n>-<topic>` bead within 24 h, and the first triage comment cites it.

**H.8 Swarm collision protocol (new, from today).** Today another agent committed this session's uncommitted working tree under its own messages and, in the same minutes, rewrote three files from older copies, dropping helper definitions whose callers had just been committed; main was red for ~35 minutes. *Change:* (a) AGENTS.md rule: never `git add -A`/`git commit -a`; stage only files you edited; (b) reserve shared monoliths (`src/lib.rs`, `src/indexer/mod.rs`, `src/storage/sqlite.rs`, `src/search/quill_bridge.rs`) through Agent Mail file reservations before editing, and install the Agent Mail pre-commit guard so a commit that includes a file reserved by another agent is refused; (c) prefer landing in small commits early over long-lived uncommitted trees. *Acceptance:* the guard refuses a planted commit touching a reserved file. *Size:* S.

**H.9 Release path (bead yviq2).** *Change:* re-register the cass dsr config with the release.yml matrix so releases and Windows receipts do not depend on manual builds.

#### WS-I — Release v0.7.2 (ES10)

**I.1 Contents:** everything in §7b plus B.1–B.4 remaining items, F.2, F.4, D.4, A.6. **I.2 Gates:** A.2 receipts (or A.1 green), full lib suite green (A.3), integration receipt (A.4), goldens reviewed, UBS clean on changed files, CHANGELOG updated. **I.3 Reporter retests:** post the prepared comments on #441, #439, #395 (drafts exist) with the exact commit and ask for `cass status --json` after retest; #440 after B.3. **I.4 Platforms:** dsr-built candidates for the five targets with the Windows receipt (B.13). **I.5 Post-release:** brew/scoop bump, crates.io publish from the clean tag, main:master mirror.

### 7.3 Dependencies and sequencing

Critical path: **A.2 → A.3 → A.4** (a trustworthy gate) feeds everything; **B.1/B.2/B.3** are independent of each other and of A but their acceptance runs need A.2; **E.1** must precede **E.6**; **C.2** must precede any C.3–C.5 work; **F.2** precedes B.2's planted negative; **B.8** precedes A.8's ratchet; **H.8** should land before any further multi-agent editing of the monoliths.

| Block | Tasks | Exit criterion |
|---|---|---|
| Day 1–2 | H.8, A.2, A.10, F.2, B.4(a), F.4, D.4, H.1 verification | one batched gate receipt green on main |
| Week 1 | A.3, A.4, A.6, B.1(a,b), B.2(a,b,c), B.5, B.8(a,b) | lib suite green; #441/#439 e2e green; WAL truncation proven |
| Week 2 | A.1 (owner), A.7, A.8, A.9, B.3, B.6, B.9(a), B.11, I.3 retest requests | CI green on push; bench ratchet live; reporters pinged with commits |
| Week 3 | B.7, B.10, B.13, G.1, G.2, H.9, C.1 decision, C.2 | v0.7.2 candidate built on all platforms |
| Week 4 | I.1–I.5 release; E.1, E.2, E.7; C.3–C.6 or E.4 per C.1 | v0.7.2 shipped with reporter retests |
| Weeks 5–8 | E.5, E.6 (rolling), F.3, G.4, G.5, B.12, C.7 | lib.rs < 30K; no dead subsystems |

### 7.4 Test, logging, and evidence standards (apply to every task)

1. **Real fixtures, real binaries.** Integration tests use the built `cass` binary and temp data dirs; no mocks stand in for the engines. Large-archive claims use the recorded fixture recipes (2.1 GB bead-cjugu clone; 15k/2M synthetic corpus builder in `tests/util/`).
2. **Positive observable + planted negative** in every test, named in the test's doc comment, plus a *No-claim* line stating what green does not prove.
3. **Receipts, not prose.** Every long-running path emits structured events (`tracing::info!` with stable field names, or an NDJSON receipt under `--robot-trace-ingest`/`CASS_PIPELINE_TRACE`) and the e2e asserts on them; bead closes cite the command and the receipt.
4. **No gate self-weakening.** Golden regeneration only after reading every diff; `#[ignore]` only with a reason naming the un-ignore condition; timeouts and thresholds change in their own commit with their own justification.
5. **Independence.** A subagent's or peer's report is a claim; re-run its cited command before closing.

### 7.5 Risks and mitigations

- **Engine dependence** (frankensqlite scale behavior, Quill merge policy): keep every cass-side bound truthful (typed refusals with hints), cross-link engine beads, and never claim an engine fix from cass.
- **Fleet admission scarcity**: batch gates (A.2); never spend an admission on a bare check.
- **Swarm collisions** (H.8): file reservations + pre-commit guard; small early commits.
- **Golden environment leaks** (A.6): isolation guard before any further regeneration.
- **Decomposition churn** (E.5/E.6): one module per commit, goldens and introspect byte-identical between commits, no behavior changes mixed in.
- **Semantic scope creep** (WS-C): nothing beyond C.2 until the owner decides C.1.

### 7.6 Effort tally (cass-side)

S: 17 tasks · M: 24 · L: 5 · XL: 2 (E.5, E.6). Roughly six focused agent-weeks for everything except E.5/E.6, which are rolling. The first two blocks (Day 1–2, Week 1) remove every user-visible failure class found in the reality check.

---

## 7b. Progress log

### 2026-09-01, gap-closing block (same day as the assessment)

Landed in the working tree (verification status noted per item; nothing below is claimed green until the compile/test receipt is recorded here):

| Item | Workstream | What changed | Proof |
|---|---|---|---|
| Robot teaching notes | WS-D5 | `emit_correction_notes`: robot mode prints `note: auto-corrected: …` on stderr; stdout untouched | `tests/cli_robot.rs::robot_mode_auto_correction_emits_teaching_note_on_stderr` |
| Bounded `health` | WS-B4 | watermark lane routed through the strict, mutation-free, 30 s-deadline owner-thread probe (`probe_state_db_strict_bounded_scoped`); inline recovery-capable lane deleted | `lib.rs::health_watermark_probe_tests` (dirty WAL byte-identical; hard failure surfaces) |
| #441 fuel + degrade | WS-B1 | `cass_quill_config()` at every Quill open (a parallel agent found the real segment-growth root cause: Quill's 1 s `max_visibility_lag_ms` sealed a segment per shard per second on cass's ingest pattern; it is now disabled so snapshots publish only on cass commits, and `force_merge` folds published runs via `concat_merge`); the fuel budget stays at the engine default with `CASS_QUILL_QUERY_FUEL_BUDGET` as an escape hatch; hybrid degrades to semantic on fuel exhaustion with `_meta.lexical_degrade_reason`; lexical-only gets an actionable hint | `quill_bridge` tests incl. `production_config_bounds_segment_growth_on_append_only_commits` (40 append-only commits must publish < 2× fan-out segments; must run) and fuel-recognition |
| #439 heartbeat | WS-B2 | post-publish FTS shadow rebuild ticks `IndexingProgress.activity` per page via `FrankenStorage::set_progress_heartbeat` | `sqlite.rs::fts_rebuild_ticks_installed_progress_heartbeat_per_page` |
| #395 TUI startup | WS-B8 | deferred (lazy) semantic loader before first frame; analytics rollup rebuild spawned as a detached `cass analytics rebuild` child | none yet (headless first-frame proof still owed) |
| Robot per-call overhead | WS-B7 | root cause measured with the v0.7.1 binary + strace: the self-heal fingerprint's sync storage open replays the whole 201 MB WAL (~4 s) on every default search; fingerprint now memoized on archive physical identity (`.archive-fingerprint-cache.json`) | `indexer::cached_archive_fingerprint_hits_on_unchanged_identity_and_misses_after_writes`; wall-time re-measure owed |
| Unsafe fence | WS-A4 | `#![cfg_attr(not(test), deny(unsafe_code))]` + scoped allows/SAFETY on 14 items; grep test replaced by fence-presence test | compile is the proof |
| Bookmarks CLI | WS-E3 | `cass bookmarks add|list|search|remove|export|import` wired to the previously dead module; exit codes 13/14 aligned with the crate table | `tests/bookmarks_cli.rs` (9 tests) |
| Docs truth | WS-D1–D4 | README/AGENTS corrected on all 15 audited items with code citations; hidden subcommands documented; today's behavior documented | review |
| Tracker | WS-H | 8 beads filed for the above; 7 stale/obsolete beads closed with commit-level evidence; landing note on k2k20 | `br` |

Dropped after investigation: `--timeout` exit-8 (main's exit-0 budget envelope is deliberate and tested; docs fixed instead); `export-html --password` (argv passwords are rejected on purpose, pinned by a test; docs fixed instead); `preferred_backend` rename (alias churn without capability).

New finding: the owner's archive carried a 200 MB WAL for 18 days; every default search replayed it. Beyond the memoization, the finalize/doctor path should guarantee WAL truncation after an index run (not yet addressed).

### 2026-09-01, evening block (Phase 2 plan, landing, and the second collision)

- **Phase 2 plan** (§7.0–7.6 above) written in place; it landed in `8269aa08` together with the README/AGENTS pass and the four schema goldens. The commit was made by a parallel agent under its own message ("feat(quill): enforce post-run maintenance segment bounding…"); the content is this session's.
- **Everything from the gap-closing block is now on `main` and mirrored to `master`** across `502dd806`, `e6300049`, `cd07dd37`, `2d341aa6`, `8269aa08` — all committed by parallel agents that swept this session's working tree. None of those messages were written here.
- **Second collision, found by checking symbols at HEAD rather than trusting the commit titles:** `cd07dd37` ("restore a compiling main") replaced the on-disk archive-fingerprint sidecar with an in-process `static` memo. A one-shot `cass search` is a fresh process, so that memo never hits on the path the fix exists for; the WAL replay was back on every default search. The sidecar layer is restored underneath the memo (`lexical_storage_fingerprint_for_db_disk_cached`, `ARCHIVE_FINGERPRINT_CACHE_FILE`), and the three dropped tests are back: `cached_archive_fingerprint_hits_on_unchanged_identity_and_misses_after_writes` (now also plants an unknown sidecar schema version as a second negative), `query_fuel_exhaustion_is_recognised_through_the_cause_chain`, `post_run_maintenance_bounds_segment_growth_on_append_only_commits`. The parallel agent's `bounded_merge_folds_session_segments_into_one` fix (CASS query parser) is kept as theirs.
- **`main` at `8269aa08` does not compile.** The batched gate (verify4, admitted 22:47) reports `E0425: cannot find value QUILL_QUERY_FUEL_BUDGET_ENV` at `src/lib.rs:28508` — `e6300049` renamed the constant to `CASS_QUILL_QUERY_FUEL_BUDGET_ENV` and `8269aa08` re-landed the lexical-only hint with the old name. Clippy, lib tests, and `cli_robot` all fail on that one error; fixed in the working tree, verify5 queued at 22:52 on the fixed tree. This is exactly the failure class WS-A.2/H.8 exist for: two agents editing `src/lib.rs` and `src/search/quill_bridge.rs` in the same hour with no reservations and no gate on push.
- **Verification receipts so far:** verify3 (earlier, batched): clippy `-D warnings` green, `cli_robot` green, `bookmarks_cli` green, golden regen/verify green, lib filter 36/38 with both reds since fixed. verify4: red on the stale constant only (nothing else ran). verify5 (fixed tree, 22:52–23:13): clippy 0, lib filter 16/16 (restored tests included), `cli_robot` 5/5, `bookmarks_cli` 9/9, goldens red on `search_robot` + `stats_full_payload` only (see the leak below).

### 2026-09-01, late block (execution of the Day 1–2 tasks)

Landed in the tree and verified by verify6 (batched, admitted 23:16, all stages 0: fmt, clippy `-D warnings`, lib filter incl. the new tests, `cli_robot` status/health, `bookmarks_cli`, `cli_dispatch_coverage` analytics, golden regen, golden verify):

| Task | What changed | Proof |
|---|---|---|
| H.8 swarm collision protocol | AGENTS.md "Swarm Commits: Stage Only What You Changed"; Agent Mail pre-commit guard installed (`.git/hooks/pre-commit`), this session's files reserved as `MaroonBluff` | guard present; rule text |
| A.2 batched gate | `scripts/gate.sh`: fmt, clippy, lib, targeted integration, goldens in one rch admission with `STAGE=<name> EXIT=<code>` receipts and refusal retries | `bash -n`; the pattern is what verify5/verify6 ran |
| F.2 exit-70 envelope | `index_stall_abort_envelope`: the standard error envelope (`kind:"index-stalled"`, code 70, `retryable`, `phase`, `stall_elapsed_ms`, `abort_threshold_secs`, phase-aware hint) printed on stderr before the bounded exit; warn-only stalls emit nothing | `stall_diagnostics_tests::stall_abort_emits_index_stalled_error_envelope_only_when_aborting` (green) |
| B.4a WAL footprint | `status`/`health` `database.db_bytes`, `wal_bytes`, `shm_present` from metadata only; introspect schema + goldens updated by intent; golden scrub for the byte values | `cli_robot` status/health green; goldens reviewed |
| B.5 first write path | `cass analytics rebuild` closes through `close_storage_with_wal_checkpoint` (the index run's checkpointing close) and reports `wal_checkpoint`; robot-docs updated | `payload_advertises_window_only_when_track_a_used_it` now asserts `wal_checkpoint == "completed"` and WAL ≤ 32 bytes (green) |
| A.6 golden isolation (a) | harness pins `PI_SESSIONS_DIR`, `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR` and sets `CASS_AUTO_REFRESH=0` | see the leak analysis below |
| A.10 fmt drift | four test files formatted; crate is `rustfmt --check` clean | verify6 fmt stage |
| H.1 verification | all 12 Pages/secret-scan beads verified against code and closed with landing commits and the exact evidence (cc7pi, z9sg6, yjjsg, kjdbv, jfcgi, h0rss, c8gx1, 1hg2q, then 7y2jt, 4ydds, 45jxv, h3ibc after a second read of the unproven items); one residual named in h3ibc's close (legacy-JSON decode path has code but no dedicated test) | `br` |

**The golden leak, root-caused (second attempt; the first was wrong).** The two goldens that absorbed a fleet worker's real Pi Agent sessions were not fed by connector detection: `isolated_search_demo_data` copies a prebuilt fixture archive and never runs `cass index`. The first hypothesis — the golden harness's own `search`/`stats` spawning a stale-on-read refresh into the fixture copy — was plausible but wrong: verify7 reproduced the leak with `CASS_AUTO_REFRESH=0` in that harness. The actual writer is `tests/cli_robot.rs`: 52 call sites use `tests/fixtures/search_demo_data` **in place** as `--data-dir` through a `base_cmd()` that never disabled auto-refresh. The refresh guard only recognizes the OS temp dir, and on a fleet worker the checkout is not under it, so a robot-CLI `search` spawned a detached `cass index --background` **into the committed fixture**, which ingested that worker's sessions (raw mirror initialized, 10 conversations); every golden test that ran later in the same job copied the dirty fixture. `base_cmd()` now sets `CASS_AUTO_REFRESH=0`; the golden harness keeps its own opt-out and the Pi env pins because the same hazard applies to the copy. The leaked goldens were restored to HEAD twice and never committed; verify8 runs the in-place robot tests before golden regeneration in one job so the proof is the goldens coming back byte-identical.

**Batch 2 (verify7 green: clippy, lib filter 42/42 incl. the key CLI tests, cli_robot 78/78, tui_flows swarm 9/9, goldens):** `src/pages/key_cli.rs` wired as `cass pages key list|add-password|add-recovery|revoke|rotate` over the complete `key_management` API (four unit tests on a real encrypted bundle, planted negatives for wrong password and last-slot revoke), `docs/RECOVERY.md` rewritten to the real surface, the README `pages key` row, the dead `Alt+W` swarm arm removed, the CHANGELOG `[Unreleased]` entry, and the robot-CLI fixture leak fix.

**Batch 3 (in the tree; verify9 pending — nothing below is claimed green):**

| Task | Change | Proof planned |
|---|---|---|
| B.5 write paths | `forget --apply`, `dedup --apply`, `analytics validate --fix` (tracks A and B) close through `close_storage_with_wal_checkpoint`; `doctor` gains `archive_wal` (pass ≤ 64 MiB; warn with the exact size and remedy; warn-and-defer while an index run owns the archive; `--fix` runs a TRUNCATE checkpoint and reports pass / blocked / failed truthfully) | `cli_doctor::doctor_reports_oversized_wal_sidecar_without_touching_it` (sparse 65 MiB sidecar → warn, read-only doctor leaves it) |
| B.4a proof | — | `cli_status::status_and_health_report_archive_footprint_from_metadata` (seeded WAL length equals `wal_bytes`; zeros without an archive) |
| B.1a | `quill_bridge::segment_file_count` (metadata-only upper bound) and `CASS_SEGMENT_PRESSURE_FILES` (32); `status`/`health` `index.segment_files` (disk footprint); doctor `index_segments` uses the engine's **live** count (`quill_bridge::live_segment_count`, files as fallback) — verify11 showed folded inputs linger on disk (40 files → 42 after a fold that left two live segments), so a file-based warning would outlive a successful consolidation | unit test `segment_file_count_bounds_the_live_segment_count_without_opening_the_engine`; the gh441 e2e judges consolidation on the live count; goldens by intent |
| B.2c | `sqlite.rs` hooks `CASS_TEST_FTS_REPAIR_PAGE_SLEEP_MS` / `CASS_TEST_FTS_REPAIR_PARK_MS` | `cli_index::gh439_slow_post_publish_fts_repair_is_not_aborted_while_it_heartbeats` (2 s per page inside a 6 s abort window → exit 0, elapsed ≥ 8 s proves it paged) and `…parked_post_publish_fts_repair_still_aborts_with_the_index_stalled_envelope` (12 s park → exit 70 + envelope) |
| B.3 | `mod.rs` hook `maybe_pause_lexical_rebuild_after_commit_for_kill` (parks after the Nth staged commit, before the checkpoint write, and records the authority/checkpoint gap) | `cli_index::gh440_plain_index_resumes_a_force_rebuild_killed_between_commit_and_checkpoint` (gap asserted from the sentinel, SIGKILL, plain `index` exits 0, every session found, no duplicate identities) |
| F.4 | `has_json_output` looks through the subcommand tree | introspect/capabilities goldens by intent |
| A.2 | `scripts/gate.sh`: `--regen-goldens`, `name:filter` integration entries, lib stage under `timeout` (`GATE_LIB_TIMEOUT_SECS`), fleet target dir decoupled from the shell's `CARGO_TARGET_DIR`; **false-green fixed**: verify8 was cut by the fleet's 30-minute SSH ceiling (rch E104) after fmt and clippy and the script still said GREEN, so a non-zero rch exit is now RED and every expected stage (plus a terminal `job-complete` marker) must report; `--lib-only` / `--no-goldens` split runs under the ceiling | verify9 (`--no-lib`, integration + golden regen) and verify10 (`--lib-only`, full suite — the receipt A.3/A.4 owe), both through the fixed script |
| B.1b | test hook `CASS_TEST_SKIP_POST_RUN_LEXICAL_MAINTENANCE` at both post-run maintenance sites | `cli_index::gh441_plain_index_consolidates_a_fragmented_generation_and_doctor_reports_it` (40 sessions, one segment each: `segment_files` > 32 and doctor warns naming the remedy; one more session + plain `cass index` → `segment_files` ≤ 32, doctor passes, every session found) |
| G.2a | `install.sh` probes host glibc (`ldd --version`) before choosing a Linux prebuilt artifact and falls back to build-from-source below 2.38 (`--artifact-url` bypasses); README sentence corrected | **proven on real hosts**: `docker run ubuntu:22.04` (glibc 2.35) warns and takes the source route; `ubuntu:24.04` (2.39) proceeds to the download; helper compare table 2.31/2.35 → source, ≥ 2.38 → prebuilt |
| D.4 | the `<mark>` highlight claim removed from the `--highlight` flag doc and README (search has no HTML output; only `**bold**` markers exist) | introspect golden by intent |
| A.9 | `scripts/validate_docs.sh --keys/--flags/--env/--truth`: README key bindings vs the TUI key map (incl. the detail pane's typed-char re-dispatch and digit ranges), every `cass … --flag` usage vs `cass … --help` (introspect describes only top-level commands — a contract gap), README env vars vs the code (connector roots via the detection crate); `gate.sh --docs-truth` stage | keys 123/123 with a planted fake key failing; the flag check found real drift (`sources sync --all`, `timeline --days`), the env check found a dead `CASS_UI_METRICS` row — all fixed |
| F.4 (jwox6) | `cass sources sync --all` exists: doctor, robot-docs recipes and the fleet rehearsal all recommended it while clap rejected it | validator + `error: unexpected argument '--all'` reproduced with v0.7.1; goldens by intent |
| F.4 unknown `--source` | **parked with the reason:** search applies the source filter before any archive handle is open, and the only truthful known-source set is the archive's `sources` table (config can be stale; analytics tests rely on an unknown id filtering to zero). Validating on the hot path would need a frankensqlite open — the WAL-replay cost B.8 just removed. Right fix: validate inside the search client where the archive is already open and surface exit 2 / `unknown-source` from there | — |
| G.1 (vdyxd) | `sources setup` runs the final sync it announces (interactive) and records `sync_complete` only after it succeeds; `--json` defers and reports `sync.status=pending`; remote install walks `candidate_methods()` and falls through on failure | `install::candidate_methods_is_the_ordered_fallback_chain`; the sshd e2e lane stays ignored (no real remote sync executed) |
| H.7 | AGENTS.md: a GitHub issue gets a bead within 24 h and the first triage comment cites it | rule text |
| H.4 (partial) | six ownerless `in_progress` beads idle since early August (ds7uy.1, gh374, gh372, gh369, gh353, gh364) returned to `open` with a dated note; the 19 stale beads and epics owned by another agent are left as that agent's claim | `br` |
| ie339 (new) | the daemon socket path (`$TMPDIR/cass-semantic-daemon-<hash>.sock`) exceeded the 108-byte `sun_path` limit on hosts with a long temp dir — the full lib suite's socket-headroom failure was a real defect, not only test coupling; the socket dir now falls back to `XDG_RUNTIME_DIR` or `/tmp` when the joined path would not fit | unit test `socket_dir_selection_stays_under_the_sun_path_bound` (verify11) |

**First full lib-suite receipt (A.3/A.4, verify10, fleet worker hz4, 2026-09-02 01:04–01:19):** `cargo test --lib` ran to completion in 438 s — **6,847 passed, 6 failed, 38 ignored, no wedge** (the bet45 packet-producer family completed). The six: one real defect from the bookmarks landing (`bookmarks` missing from `CANONICAL_TOP_LEVEL_COMMANDS`, so robot mode rewrote `cass bookmarks … --robot` into a search; fixed in the tree, re-run in verify11); four `storage::sqlite` tests that shell out to a `sqlite3` binary the worker does not have (legacy duplicate-FTS-schema repair and historical-bundle seeding); one daemon socket-path headroom test that fails under the worker's long checkout-relative temp path. The last five are test-environment coupling, filed as bead hyqjz, not product defects proven by this run. In the tree since: the four `sqlite3`-dependent tests return early with a printed `SKIPPED` reason when the binary is absent (a disclosed narrowing — on the fleet they now prove nothing; on hosts with `sqlite3` they run in full; the durable fix is routing that repair through frankensqlite). The socket-path failure turned out to be a real defect (bead ie339, fixed in the tree — see the batch-3 table): a long `$TMPDIR` pushed the daemon socket path past the Unix limit, so on such hosts the daemon could not bind at all.

**Measurement (nsleh denominator, 2026-09-02 00:12, owner's archive, v0.7.1 binary, `search "borrow checker lifetimes" --robot --robot-meta --limit 5`, five warm runs):** median wall 15.3 s; `search_ms` 6.3–7.5 s (engine, 269 segment files); `other_ms` 1.0–1.9 s; the remaining ~7 s is the synchronous archive open replaying the 200 MB WAL. **Main-build re-measure (2026-09-02 05:40, same archive, same query, same 267 segment files and 200 MB WAL, release build of this tree, five warm runs):** median wall **1.9 s** (runs: 3,798 ms cold with the one-time fingerprint-sidecar miss, then 1,829 / 1,820 / 1,897 / 1,933 ms); `search_ms` 337–406 ms; `other_ms` 754–810 ms after the first run's 2,599 ms. Countermetric: `hits` stayed 1 for both binaries, so the 8× drop is not a result-set change. What is *not* isolated yet: engine time fell from ~7 s to ~0.35 s with the segment count unchanged, so the segment fragmentation was not the dominant cost on this archive; the likeliest cause is the commit-only Quill snapshot config (no per-open seal), which needs its own A/B before GH #441's comment claims it. The consolidation effect is measured separately after the incremental `cass index` on this archive (owner-index receipt below).

**Owner-archive index run (2026-09-02 05:44, main build, `cass index --json --no-progress-events`, the documented consolidation path) — DID NOT COMPLETE; new P0 finding.** The run never left `preparing`: `stall_detected` at 120 s with `pre_index_io_active=true`, lock phase `watch_startup:classify_nonresumable_checkpoint` (a stale breadcrumb: that function only reads a JSON sidecar; the step actually running is `open_storage_for_index`, i.e. frankensqlite's *writable* open), one core at 99 %, RSS 6.7 GB flat, and `strace` showing ~30,000 `pread64(…, 24 bytes)` per second on `agent_search.db-wal` in short backward walks (~30 frame headers each, ~1,000 walks/s, offsets spanning the whole 200 MB file), `read_bytes` flat (page cache), `rchar` +19 MB/s. Killed at 7 min 38 s (rc 143) with no forward progress. `cass doctor --fix` (the remedy the new `archive_wal` check prescribes) enters the same loop right after its `archive_db_open_integrity` phase. Read-only opens are unaffected: `status`/`doctor --json`/`search` on the same files take ~1.5 s. Context that makes this P0: `status.index.last_indexed_at` = **2026-08-14** — the owner's own archive has not completed an index run in 19 days; `agent_search.db` mtime 08-26, the 200 MB WAL untouched since 08-26, so every stale-on-read auto-refresh since then has died in this open. Not the capped WAL page index (`PAGE_INDEX_MAX_ENTRIES = usize::MAX` in fsqlite-core 0.3.13, 0.3.14 and 0.3.15), so `index_is_partial` cannot be the trigger; the exact loop is unidentified (stripped release binary). Handed to the frankensqlite side with the strace signature (bead kupq4 lineage / GH #382). A lossless-checkpoint experiment on a *copy* of the archive with C SQLite is recorded below; the live archive is not modified by anything in this session.

**Copy experiment (05:58–06:07, `scratchpad/wal-copy-experiment.sh`, `copy-index.sh`, `copy-integrity.sh`; `agent_search.db` + `-wal` copied, never the live files):** C SQLite 3.46.1 opens the copy in WAL mode and reads through frankensqlite's WAL: 1,012 conversations / 538,807 messages / `max(messages.id)` 538,807 (frankensqlite's own `status` counts were null on this archive; the row counts were not cross-checked against a frankensqlite read). `PRAGMA wal_checkpoint(TRUNCATE)` returned `(0,0,0)` in **1.7 s**, the WAL was removed, and every count was identical afterwards — the checkpoint is lossless at row level. `PRAGMA quick_check` (17.5 s) and `integrity_check` (7 s, 103 lines) both report **pre-existing page-level damage**: `Freelist: 2nd reference to page 262145`, `free space corruption` on 33 pages across trees 1, 4, 29, 45 and 60, and 70 `never used` pages (8196–8275). By cass's own doctrine in `DOCTOR_FTS_TABLE_QUERYABLE_MESSAGE` (cass#438) stock `integrity_check` is authoritative at the page/b-tree level, so this is real damage from an earlier frankensqlite writer (the 0.3.13 pin carries the cass#434 corruption-writer fixes; the archive predates it) — and `cass doctor --json` never looked: its `database` check is `warn` with "structural integrity is unchecked; frankensqlite_full_page_integrity_probe_deferred: archive bundle is 10,490,719,496 bytes, above the bounded doctor limit", while the top-level `status` still says `healthy`. That is a declared skip plus a misleading summary status (an earlier draft of this paragraph, the commit message of 5f059384 and the first version of the #382 comment called it a false negative; corrected 2026-09-02 10:35). The gap: doctor needs a bounded page-integrity probe that still runs on large archives (sampled or time-boxed), and a top-level status that is not `healthy` while integrity is unchecked. On the checkpointed copy, `cass index --data-dir <copy>` **gets through the writable open**: `preparing` still took 376 s (two report-only stall events at 120 s; the copy has no fingerprint sidecar, so the strict full-archive fingerprint runs) and then `scanning` → `indexing` with all 1,012 conversations loaded, at 14.3 GB RSS while indexing. So the 200 MB WAL is the trigger for the endless writable open; the freelist damage did not stop the copy run. **Remedy (operator decision, not taken here):** back up `agent_search.db` (10.3 GB copy, 40 s) plus `-wal`/`-shm`, then checkpoint the live WAL with stock SQLite (`sqlite3 agent_search.db "PRAGMA wal_checkpoint(TRUNCATE);"`, proven lossless on the copy, ~2 s), then run `cass index` and let the post-run fold consolidate the segments; separately decide whether to rebuild the archive (`VACUUM INTO` a fresh file, then swap) to shed the freelist/free-space damage before the next frankensqlite writer touches those pages. Upstream: the writable-open loop on a large WAL needs a frankensqlite reproducer (`scratchpad/frankensqlite-wal-finding.md` has the strace signature and the ruled-out cap path).

**Sidecar bisection (2026-09-02 10:25–10:55, `scratchpad/wal-bisect/run3.sh`, fresh copies, live files untouched):** two copies of the archive, one with every sidecar (`-wal`, `-shm`, `-wal-cert`, `-wal-cert-head`, `-fsqlite-ns-use`, `-fsqlite-ns-gate`) and one with **only the `-wal` file**, both under `cass index --data-dir <copy>`. Four minutes in, a 4 s `strace` sample showed the identical signature on both: 147,488 and 163,582 24-byte `pread64` calls on the WAL fd (≈ 37–41 k/s), i.e. the loop, with neither leaving `preparing` in 900 s; the WAL-free copy from the earlier experiment left `preparing` after 376 s. Conclusion: **the WAL alone (48,607 frames, 200 MB) triggers frankensqlite 0.3.14's endless writable open (the pin since 5acc0dc6 on 2026-09-01; my earlier paragraphs said 0.3.13 — the lockfile and the measured binary carry 0.3.14); no sidecar is involved.** Two earlier rounds were inconclusive by construction (a 300 s cap is shorter than the legitimate 376 s open; a 75 s sample lands before the loop engages) and are kept in `run.log`/`run2.log` for honesty. Remaining upstream question: which loop — a symbolized `fsqlite` CLI build from the local 0.3.15 checkout is in flight to (a) test whether 0.3.15 still loops on the same copy and (b) sample it with gdb.

**Root cause (10:51, gdb on the symbolized 0.3.15 `fsqlite` CLI while its `PRAGMA wal_checkpoint(PASSIVE)` showed the identical syscall signature):** `fsqlite_pager::pager::checkpoint` (pager.rs:27153) → `reclaim_disowned_in_range` (26798) walks the per-file disowned-page ledger (`abandoned_eof_reservations`; **1,576,443** entries on this archive, left by the `repair_orphaned_pages:1576443` migration recorded in the marker on 08-24) and asks `WalBackend::read_page_at_appended_tail` (wal_adapter.rs:4339 → 1687) for each page, which by design (bd-dw8oe) runs `scan_backwards_for_page` (642): a 24-byte `read_frame_header` for every WAL frame from the tail down. Cost = ledger × frames = 1.58 M × 48,607 ≈ 7.7 × 10¹⁰ header reads. The on-open reclamation sweep (bd-ioq6x Face-2, GH#346) uses the same path, which is why cass 0.3.14's writable open loops and why the WAL-free copy's open took exactly the 376 s of a ledger-only sweep. Round 4 (with the migration marker copied): cass 0.3.14 still loops (138,537 header reads in 4 s); the 0.3.15 CLI's plain SELECT (129 s) and `BEGIN IMMEDIATE` (43 s) complete, its checkpoint loops. **Fix, in tree at `/data/projects/frankensqlite` (`crates/fsqlite-core/src/wal_adapter.rs`):** `AppendedTailIndex` — index the appended tail once per stable tail (generation identity + frame count + tail-frame checksum) and answer `read_page_at_appended_tail` from the map, identical semantics (newest frame wins); `appended_tail_frame_for_page`; test `appended_tail_reads_index_the_tail_once_per_stable_tail`. Gate + a rebuilt CLI are on the fleet; the acceptance proof is the same checkpoint on a fresh WAL copy completing. frankensqlite's own `br` database is malformed (`table_seek called on index page`), so the upstream bead could not be filed; the commit cites cass bead g3zyo. cass consumes the fix via the next crates.io release (build.rs rejects registry patches), so the cass-side change is a pin bump once it ships.

**Block 5 (2026-09-02 16:25+, disk reclaimed by the owner): pin bump to fsqlite =0.3.15 (bead yxcga, gh382-fsqlite-pin).** Rationale: on the owner's archive 0.3.14's writable open never returns, while the 0.3.15 CLI's open + `SELECT` (129 s) and `BEGIN IMMEDIATE` (43 s) complete on the same copy; 0.3.15 also carries the FTS5 `optimize` migration and contentless data-safety fixes. Not a fix for the checkpoint loop (that is `8d012706a`, next release). Changes: `Cargo.toml` (3 pins), `build.rs` (3 `expected_version` + the stale 0.3.13 comment), `AGENTS.md` dependency row, `Cargo.lock` resolved on the fleet (20 fsqlite-family crates, nothing else). Gate: verify18a (fmt, clippy, full lib suite) + verify18b (cli_index gh439/gh440/gh441, cli_doctor, cli_status, pages_key_cli, storage_frankensqlite_parity, cli_robot subset, goldens checked without regeneration). Acceptance beyond the gate: a release build of cass on 0.3.15 must leave `preparing` on a fresh copy of the owner archive where the 0.3.14 build (kept as `main-bin/cass-0.3.14`, the planted negative) still loops. The J2 acceptance probe for `8d012706a` (fixed CLI checkpoint on a fresh copy) is running in parallel.

**Outcome (17:50): NOT ADOPTED.** (1) J2 passed for the upstream fix: the fixed CLI's `PRAGMA wal_checkpoint(PASSIVE)` on a fresh copy returned `0 | 48607 | 48607` in 53 s with 538,807 messages intact. (2) The 0.3.15 cass release build (K) still loops in the writable open on the copy: 126,632 24-byte WAL header reads in a 4 s window at 5 min, phase `preparing`, and both K (0.3.15) and K0 (0.3.14, the planted negative) were cut by their 1,200 s cap in `preparing` with zero conversations indexed — the CLI probe's clean `SELECT`/`BEGIN IMMEDIATE` did not transfer to cass's open path, so the bump buys g3zyo nothing; only the release carrying `8d012706a` does. The +5 s open cost was isolated with `strace -tt`: every ≥ 0.5 s pause in both traces is a 1.00 s wait right after `fsync` (a durability barrier); 0.3.14's run has two, 0.3.15's four plus a 0.9 s one, i.e. two extra durable commits at open on a fresh archive (FTS5 optimize migration / contentless self-heal are the candidates). Both versions also reopen `agent_search.db-wal` 1,070 times during a two-session index run (per-statement WAL path refresh) — pre-existing, noted upstream. (3) Local A/B on the two release builds (2-session fixture, gh439 env, 3 rounds each): first non-preparing progress at 2.0–2.8 s on 0.3.14 vs 6.1–7.7 s on 0.3.15 — a ~5 s open regression whose cause is not identified (migration-on-open or a 5 s busy wait are the candidates); it is what tripped `gh439_slow`'s 20 s abort on the loaded fleet in verify18b. (4) verify18b was otherwise green on 0.3.15, goldens included; verify18a's two lib failures came from another agent's transient test in the synced tree and a fn-pointer-identity registry test, re-run as verify18c for the record. Pin files reverted to `=0.3.14`; `AGENTS.md`, `CHANGELOG.md` and `build.rs` record the evaluation; bead yxcga stays open for the 0.3.16 bump. Also landed with this block: the index run's final WAL checkpoint is bounded (`close_storage_after_index` → `run_bounded_abort_wal_checkpoint`, 900 s default, `CASS_INDEX_FINAL_WAL_CHECKPOINT_TIMEOUT_SECS`), the doctor deadline reuses the same worker, and `gh382_final_wal_checkpoint_is_bounded_and_leaves_the_wal_for_the_next_run` pins it — after four gate cycles spent on my own test shapes (an absolute 15 s then 45 s wall bound that a loaded debug worker cannot meet; the assumption that frankensqlite's TRUNCATE leaves a 0-byte WAL when it keeps the 32-byte header). The final shape is relative: a 60 s park against a 1 s budget must finish within the unparked run's time plus 30 s, leave frames in the WAL, and the next unparked run must shrink it to the header. `gh439_slow_post_publish_fts_repair_is_not_aborted_while_it_heartbeats` was reshaped too (12 s/page = 48 s of work under a 40 s window) because its old 16 s-under-20 s shape could not discriminate heartbeats and tripped on `preparing` alone — a gate change, reviewed as such. Receipts: verify20 (gh439 slow + parked green, doctor green) and verify23 (gh382 green, doctor green), fmt and clippy green in both.

**Block 6 (2026-09-02 19:00–20:20): the remedy rehearsal and what it found.** On a fresh copy of the owner archive (`J2`), the fixed fsqlite CLI's `PRAGMA wal_checkpoint(TRUNCATE)` completed in 47 s (`0 | 48607 | 48607`), and today's cass (the 0.3.14 release build) then **got through the writable open** — the remedy path works: `preparing` took ~6 min (the ledger-only reclaim sweep), then `scanning` → `indexing` at roughly 25 conversations/min. It **did not finish**: at conversation 88 of 1,012 the run stopped making progress — one `cass-index-work` thread at 80–100 % CPU in user space, ~0 file I/O (115 syscalls in 4 s, all pipe/eventfd reads), `producer_state: idle`, queue 0, a report-only `stall_detected` every 120 s, RSS 13–16 GB (HWM 16.3 GB). That is a CPU-bound step on one conversation, not the parked-pipeline deadlock GH #413 (bead cjugu) describes, though GH #413 notes the same onset (the first ~250 MB conversation, where `redact_secrets` skips memoization). Root cause needs a symbolized stack: a `profiling`-profile cass is building on the fleet and `wedge-probe.sh` (fresh copy, truncate, index under gdb sampling) follows. **Two operational facts from this block, both mine to own:** (1) the session's own cgroup scope caps memory at 16 GB — the 16 GB cass run tripped it at 19:36:46 and the kernel OOM-killed the Claude session itself (dmesg: "Memory cgroup out of memory: Killed process (claude)"), taking every background job with it; archive-scale runs must be launched in their own `systemd-run --user --scope -p MemoryMax=…`; (2) the session scratchpad is purged by age — after the restart every file not touched within the hour was gone (release binaries, archive copies, all verify-1…26 receipts and logs, scripts, the finding note); the numbers those receipts carried survive only in the commit messages and this document. Also this block: the fingerprint sidecar's temp file made in-place robot tests race fixture clones (5 failures; clone now skips it, bead zgzva), and the two pack-timeout robot tests were reshaped because their 500 ms budget expired inside `search_setup` on the debug fleet build (2 s budget / 5 s injected stall / 4.5 s guard); the registry test is behavioral and the orphan-FK env test serial (swept into 6362634a by another session before its gate; green in verify25/27).

**Wedge root cause (21:50, gdb on a symbolized cass — release, LTO off, debug symbols — on fresh copy K2 after the fixed-engine `TRUNCATE`, run inside a `systemd-run` scope):** the "wedge" is quadratic work, not a deadlock. The hot thread's stack, top to bottom: `fsqlite_ext_fts5::Fts5Table::snapshot_state` (a deep clone of the entire in-memory FTS5 table state: inverted index, documents, shadow rows, row locales) ← `Fts5Table::savepoint` ← `fsqlite_core … begin_live_vtab_transaction_if_needed` / `live_vtab_savepoint_all` ← `with_internal_statement_savepoint_and_cx` (frankensqlite wraps every statement in an internal savepoint) ← `execute_live_vtab_insert_rows` ← `Transaction::execute_with_params` ← cass `storage::sqlite::franken_batch_insert_fts` (sqlite.rs:16788) ← `flush_pending_fts_entries` ← `insert_conversations_batched_with_analytics` ← `persist_conversations_batched_inner`. Every `INSERT INTO fts_messages` statement therefore clones the whole FTS table: O(|fts_messages|) per statement, minutes each at 538,807 rows, with a 35 GB transient (RSS HWM 34.9 GB). The onset moves with table size (conversation 46, 59 and 88 in three runs), which is exactly GH #413's "wedges at a batch boundary" and explains why its 250 MB conversation looked causal. Filed upstream as a frankensqlite GitHub issue (its beads database is malformed) with the trimmed stack; cass bead cjugu updated and claimed. **Fix A (cass, now):** route the FTS inserts through `execute_with_params_skip_statement_savepoint` — the API already exists in `franken_sync` with zero callers, and the batch transaction is the rollback boundary — which removes the per-statement clone; the per-transaction `begin` clone remains. **Fix B (frankensqlite):** an undo log per savepoint level instead of `snapshot_state`. A second cass site to evaluate after measuring A: the paged fallback-FTS rebuild's explicit `SAVEPOINT cass_fts_rebuild_page` (sqlite.rs:13807) hits the same clone once per page.

**Fix A landed and measured (22:04 on main as f35f25d0 via another session's sweep, 34 min before its gate; receipt verify32 GREEN 22:38: fmt, clippy, storage fts/insert/batch/flush lib tests 31 passed, cli_index gh439/gh441, storage_frankensqlite_parity 38 passed; changelog 3ae2e595). K3 (23:39–23:50, fix-A symbolized build on a fresh checkpointed copy in a 40 GB scope): NEGATIVE for throughput on fsqlite 0.3.14.** The per-statement FTS5 clone is gone from the profile, but the run stalls at conversation 2 (K2 unfixed had reached 46–59 by the same time) at RSS HWM 34.5 GB, inside the batch transaction's COMMIT: `fsqlite_pager::pager::commit` → `resurrected_or_erased_freelist_pages` (pager.rs:10218) → `read_durable_page_under_gate` (10122) → `read_page_at_appended_tail` → `scan_backwards_for_page` — the same per-page backward WAL header walk as the open/checkpoint loop, which frankensqlite `8d012706a` already fixes (its `read_page_at_appended_tail` now resolves through the appended-tail index, so every caller including commit benefits) but which is unreleased. So this archive carries three quadratic terms: (1) the per-statement FTS clone — fixed in cass; (2) the WAL tail rescan on open, checkpoint and commit — fixed upstream, unreleased; (3) the per-transaction FTS `begin` clone — frankensqlite#405, open. f35f25d0 is necessary and stays, but nothing on the cass side unblocks this archive until the fsqlite release with `8d012706a` ships and the pin moves (bead yxcga); then re-measure. `CASS_DEFER_LEXICAL_UPDATES=1` was ruled out as an operator lever: it skips the inline FTS writes (harmless: the SQLite FTS lane serves queries only when the Quill index is unavailable) but also skips the inline Quill updates and forces a full authoritative Quill rebuild at the end of every run. Posted on #413 and on frankensqlite#405.

**Block 8 (2026-09-03 00:00–01:30): the third term fixed upstream, and the doctor summary made honest.** frankensqlite#405 is fixed on frankensqlite main (`f68fa54c1` + tests `afe829f4b`, fmt `6ed147c30`, clippy `fc6a144fa`, changelog `29951b77d`; all swept there by another session within minutes of the edits, before any gate): `Fts5Table` keeps an `Fts5UndoLog` instead of a `TransactionalVtabState<Fts5TableSnapshot>` — `begin`/`savepoint` record only the small table header plus a log position, each row mutation appends one inverse op (an insert its rowid; an update or delete the previous content, locales and that row's own postings via `InvertedIndex::document_postings`), and only the bulk rewrites (shadow-row materialization, contentless hydration, rebuild, bind) still record a full snapshot; `rollback`, `rollback_to` and `release` keep their observable semantics. Gate (fleet, one admission): fmt 0, clippy pedantic+nursery 0, `fsqlite-ext-fts5` lib 332 passed (three new tests: inserts record row ops and no snapshot; `ROLLBACK TO` restores replaced/deleted rows with locales, column sizes and the rowid watermark; bulk rebuild still rolls back via snapshot), `fsqlite-core` fts5/vtab/savepoint subset 163 passed in a fresh target (the shared target failed to link with stale artifacts first). **Two archive-copy A/Bs were invalid by construction and are not evidence:** the fsqlite CLI opens the contentless `fts_messages` lazily (or, with `FSQLITE_DISABLE_LAZY_FTS5_REBIND=1`, rebuilds an empty index from a non-existent `_content`), so no populated-index clone ever ran on either engine. The valid A/B uses a synthetic content-backed 500k-row table, where the engine promotes the lazily opened table by rebuilding the whole index on first write and the pre-fix engine then clones it per statement. Result (100k rows, ten 10-row INSERTs in one transaction, each engine on its own copy, `/usr/bin/time`): pre-fix 45.0 s and 1,358 MB peak RSS; fixed 41.7 s and 971 MB peak RSS. The clone is gone — the peak-memory drop is the doubled table no longer being built — but the wall time barely moves, because the same build exposed **a fourth O(table)-per-statement term that has nothing to do with savepoints**: gdb on the fixed engine during the load shows every INSERT statement re-persisting the whole fts5 shadow (`persist_rootpage_zero_fts5_insert_rows` → `replace_storage_table_rows` rewriting all rows through the btree, and `to_segment_leaves`/`Fts5PendingDoc::to_poslist` re-encoding all segment data). That is why the 500k build never finished in an hour (the WAL stayed at 28 KB: nothing had reached commit) and why the 100k build took 197 s. Filed upstream as frankensqlite#406 with both stacks; on the cass side it is the same class as GH #345/#369 (FTS shadow rebuilds on large archives) and is now the dominant cost of a full FTS rebuild. Honest summary of #405: it removes one quadratic term and its memory doubling; it does not make bulk fts5 loads fast. cass cannot be built against the local engine (build.rs rejects fsqlite `[patch]` sources), so the cass-level proof waits for the release and the pin bump (yxcga). Also this block: `cass doctor --json` now reports `reason_code: "integrity_unchecked"` when the deep page-integrity probe was deferred (the "healthy while unchecked" shape that misled the 09-02 reading of this archive), with an end-to-end test that plants the deferral on a symlinked archive and a planted-negative regular file. The feature and the first shape of the test were swept to main (63d47293) while that shape was red (verify33: the seed's cached integrity attestation, keyed on size and mtime, answered for the deferred probe); the fixed shape bumps the archive mtime and is green in verify34 (fmt, clippy, cli_doctor integrity_unchecked + oversized_wal + wal_checkpoint deadline, goldens checked), landed as 17b747e6.

**Block 9 (2026-09-03 13:10–14:08, observed read-only at 17:30; not run by this session): the installed v0.7.1 auto-refresh ran on the live archive, and it corrects a word in this document.** A stale-on-read auto-refresh (`auto-refresh-state.json`: `last_reason: index-stale`, spawned 13:10:03 from the reading process's own `current_exe`, i.e. the owner's installed `~/.local/bin/cass` 0.7.1 = fsqlite 0.3.13) ran `cass index --background` against the live archive with its 200 MB WAL. The `auto-refresh.log` phase events: `preparing` from 13:12 (first stall warning at 120 s) until the first `indexing` event at 13:49 (`elapsed_ms` 2,340,181) — the writable open plus preflight took **30–39 minutes and completed**; a lexical generation was published at 13:38 (`index/v9-quill`, `.lexical-publish-backups`), the WAL was autocheckpointed to 4,152 B at 14:05:58 (`-wal-cert` rewritten 14:06:10), and the run then crawled the fts5 write path with "no indexing progress for 120s" warnings at 159/177, 177/188 and 188/191 conversations until the memory cgroup killed it at 14:08:40 (`dmesg`: `Memory cgroup out of memory: Killed process 897803 (cass) ... anon-rss:14208436kB`, `oom_memcg=/user.slice/user-1000.slice/user@1000`). `last_indexed_at` is still 2026-08-14; the Quill directory now holds 1,422 `seg-*` files with 838 live (the dead run's segments are orphans until a run completes); `quarantine/connector_ingest_lines.json` was written 13:41. **Correction:** Blocks 3–5 say 0.3.14's writable open on this archive "never returns" / "loops". Every run this session made was capped at 7–20 minutes; this run shows the open finishes in roughly half an hour on the 200 MB WAL (the K2/J2 copies without the WAL opened in ~6 min; the fixed engine `8d012706a` checkpoints it in 53 s). The mechanism (`reclaim_disowned_in_range` rescanning the appended WAL tail per ledger page, O(ledger × frames)) and the fix are unchanged; the word "never" was wrong and is withdrawn here, on beads g3zyo/cjugu and on GH #382. The 0.3.13 engine has the same sweep (`fsqlite-pager-0.3.13/src/pager.rs` carries `reclaim_disowned_in_range` and the bd-ioq6x on-open sweep), so v0.7.1 users are affected by the same open-time term. **Operational consequence for the owner:** every stale read (any `cass search`/`status` without `CASS_AUTO_REFRESH=0`) respawns this hour-long run, which ends in the cgroup OOM (the fts5 per-statement clone, cass fix A `f35f25d0` and frankensqlite#405, is not in v0.7.1) and leaves more orphan segments each time. Until a cass release carries fix A and a frankensqlite release carries `8d012706a` + `f68fa54c1`, the honest advice is `CASS_AUTO_REFRESH=0` (or `cass schedule uninstall`) on this machine; a `cass index --rebuild-lexical` is not needed — the live WAL is now small, and the next completed incremental run will reclaim the orphan segments. Nothing in this block was run by this session; the live files were only read.

**Block 10 (2026-09-03 17:45–23:35): "fix this for real" — the auto-refresh breaker, the FTS budgets, the finding that changed the diagnosis, and the finding that changed the priority (bead iify0; bead scohn).** The owner rejected the block-9 advice ("set `CASS_AUTO_REFRESH=0`") as a non-fix. Two cass-side fixes were built first: (1) `background_refresh` judges the previous auto-spawn against the index watermark the read path already holds — a spawn the watermark did not outlive counts once as a failure, spacing escalates 1 h then 6 h, three in a row trip the breaker until any run advances `last_indexed_at`; surfaced as `auto_refresh.outcome` `backed_off`/`tripped` in `--robot-meta`/`status --json`, in `schedule status`, and as a `doctor` warn check; (2) inline `fts_messages` flushes are charged against a per-run budget (`CASS_FTS_INLINE_BUDGET_SECS`, 300 s) and the paged repair gets a per-page budget (`CASS_FTS_REPAIR_PAGE_BUDGET_SECS`, 120 s), both leaving the shadow Partial with the reason in the existing repair-pending marker. **Then the acceptance runs on a fresh copy of the owner archive overturned the diagnosis.** L1 (24 GB scope, the cap that killed the owner's child): OOM-killed in `preparing` after 3 min at 25 GB anon RSS, before any fts5 write. L2 (64 GB scope, RSS sampled, gdb at 12 GB and at 60 min): both samples are `fsqlite_core::connection::rebuild_materialized_live_vtab_instances_from_reload → Fts5Table::rebuild_documents → index_document_with_tokenizer` (docid 515603, then docid 415749 an hour later under a read-only analytics chunk query) — fsqlite 0.3.14 re-tokenizes the whole 631,657-row contentless shadow into RAM at **every** memdb reload after a dirty transaction, about 2 minutes and 20 GB each time, so the run spent an hour in `preparing` at a flat 26 GB with zero conversations processed while the analytics rebuild's per-chunk heartbeat kept the watchdog quiet (peak RSS 30.9 GB, stopped at 61 min). The Quill full rebuild inside that window took 84 s for 550,644 docs (156 s on the owner's machine at 13:38). Filed as frankensqlite#408 with both stacks (the third O(table) term on this table after #405/#406). **Consequence:** the inline budget only stops the write crawl; the shadow's mere existence costs the hydration. Third fix: `CASS_FTS_SHADOW_MAX_MESSAGES` (default 100,000 indexable messages, the exact index-driven count; the first draft summed `daily_stats.total_chars`, which is 436 MiB on this archive and would have passed a byte bound) — the index run drops an oversized shadow at preflight through the deferred-FTS5 connection (nothing hydrated), a run whose ingest crosses the bound stops feeding it and drops it at finalize, `index --full`/`doctor --fix` refuse to recreate it while the corpus is over the bound (nonfatal `SkippedNotViable`, reason persisted), `doctor`'s `fts_table` check says the drop was deliberate; Quill is the lexical engine, the SQL fallback already scans `messages` when no shadow exists. Two more defects the tests caught before acceptance: the suspension state lived on the pool-writer connection and was invisible to the primary's finalize (no marker persisted) — it now lives in an `Arc<FtsShadowRunState>` shared through `new_with_shared_caches` like the ensured-row caches; and the catalog probe filtered `sqlite_master` on `rootpage > 0`, which fsqlite virtual tables never satisfy, so the drop could never fire (07e77021). Meanwhile another session bumped the engine pin to fsqlite 0.3.16 (96fc1e88, "unblocks g3zyo engine-side": 8d012706a, the #405 undo log, the #406 segment append, lazy contentless FTS5 on the ordinary open) but missed the family convergence constant in `build.rs`, so main did not build for half an hour (fb3f7468). Sweeps #8–#11: another session committed this block's working tree four times before any gate (744b50ab…509ff70e; 879b0f88/08cb5a21; f5a7c0ec/1aade2a8; 07e77021). While the block ran, the live archive received two more doomed catch-ups from other agents' reads of the installed v0.7.1 (a run that wrote an 81 MB WAL by 19:57; pid 173018 spawned 20:33 in the owner's terminal-mux scope with no memory cap, stall-aborted with exit 70 at 21:30 at conversation 197 of 216). Receipts: verify46a (fleet, scripts/gate.sh --lib-only, HEAD fb3f7468, 22:33-22:45): fmt 0, clippy 0 (--all-targets -D warnings), lib-tests 0 (27 passed: background_refresh, fts_shadow, doctor_fts_probe); verify46b (--no-lib --regen-goldens, 23:09-23:33): fmt 0, clippy 0, test-cli_index 0 (gh413_inline, gh413_paged, gh413_fts_shadow_over_its_corpus_bound, gh382_final: 4 passed, single-threaded), goldens-regen 0, goldens 0 - GREEN. The reds on the way (verify35, 37, 38, 43: this block's own test shapes, the swept intermediates, the rootpage probe) are on bead iify0. **Acceptance L3/L3b (cass fb3f7468 on fsqlite 0.3.16, 24 GB scope, fresh copies of the live archive):** the shadow bound does what it says — preflight drops the 644k-message shadow through the deferred-FTS5 connection, `fts_messages` and its five shadow tables leave the catalog, both markers persist, `status` carries `index.fallback_fts_repair`, `doctor` stays healthy, search answers through Quill, 3:00 wall, 17.7 GB peak inside the cap — and then the run fails, both times, on a malformed page. The first copy was torn (taken while the third child checkpointed into the live file at 20:36:49). The second copy was taken quiescent and checked before the run: **the owner's archive is corrupt before any iify0 code runs.** fsqlite 0.3.16 `integrity_check` (doctor --check, full-page probe forced): "page 262145 referenced by freelist trunk[1876] leaf[194] is the reserved lock-byte page"; the L3b run died on "page 151751 was freed earlier in concurrent transaction 45"; C SQLite `quick_check` independently reports the freelist double reference, 68 doubled child references (conversations root 29 page 139235 cells 264/266/268 → pages 159466/159499/159500; messages; the messages autoindex) and a child-depth mismatch in `messages`, none of which the untouched 08-24 pre-migration backup shows (its 32 "free space corruption" findings recur on the same page numbers in today's file and are a page-bookkeeping dialect, not damage). The backup's freelist is clean by C SQLite's reading, so the lock-byte entry appeared between 08-24 and today under the 0.3.13/0.3.14 writers that were killed mid-transaction; fsqlite's own `quick_check(1)` on the backup returns several rows that cass doctor could not render (bead: doctor multi-row quick_check). Filed upstream as frankensqlite#410. Bead scohn (P0) carries the facts; the owner was notified twice (stop writers, back up; then the engine confirmation). No incremental run can be shown to complete on this archive until it is recovered; the iify0 change is validated on everything it controls and blocked on that.

## 8. Answers to the five reality-check questions

1. **What IS working:** the whole lexical core loop, robot ergonomics, doctor v2, scheduler, raw mirror, redaction, daemon attestation, packaging, Pages (hidden), and the storage safety story (no unsafe Send/Sync, rusqlite gone).
2. **What is NOT:** progressive/two-tier semantic search (does not exist at runtime), ANN readiness, bookmarks, swarm live composition, `--timeout` partials, bounded `health`, setup's final sync, installer glibc gate, self-update rollback, and — most importantly — index/search reliability on multi-GB append-only archives (#439/#440/#441/#395/#391/#379/#345/#329/#349/#320).
3. **What is blocking:** engine-boundary scale behavior, no CI ratchet, the lib.rs/indexer monolith, a swarm process that closes beads faster than it tracks user issues, and an ownerless two-repo semantic program.
4. **Would all open + in-progress beads close the gap?** No — they cover the semantic architecture (if executed), Windows, rustsec, some engine-blocked memory items and Pages hardening, but none of the §5.1 list (today's three issues, corruption detection, CI, docs truth, dead code, structure, bookmarks, timeout contract, per-call overhead).
5. **Vision goals with zero bead coverage:** see §5.1 — 17 items, led by #441/#439/#440, #395 residual, #391, CI re-enablement, README truth, and lib.rs decomposition.
