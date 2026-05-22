# Changelog

All notable changes to **cass** (coding-agent-session-search) are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) with links to representative commits.
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Repository: <https://github.com/Dicklesworthstone/coding_agent_session_search>

> **Releases vs. tags**: Published GitHub Releases with downloadable binaries
> are tracked on the [Releases
> page](https://github.com/Dicklesworthstone/coding_agent_session_search/releases).
> Not every version below has release artifacts; entries without a GitHub
> Release are source tags only.

---

## Unreleased

## [v0.5.2] -- 2026-05-21

**Data-loss fix: stop v0.5.1's quarantine system from permanently dropping active sessions and small JSONLs.**

Two coupled fixes for [#251](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/251), where 63% of `cass index --watch` ingest attempts on macOS were being permanently quarantined as "out of memory", including 2.4 KB files and the JSONLs Claude Code was actively writing to.

- **OOM detection is now typed, not a substring match.** `is_out_of_memory_error` (`src/storage/sqlite.rs`) and `error_is_out_of_memory` (`src/indexer/mod.rs`) walk the `anyhow::Error` chain and look for `frankensqlite::FrankenError::OutOfMemory` via `downcast_ref`, mirroring the existing `retryable_franken_anyhow` pattern. The previous heuristic — `error.to_string().to_lowercase().contains("out of memory")` — caught every error chain whose rendered text included that phrase, including frankensqlite's typed `OutOfMemory` variant emitted for non-process-OOM internal conditions (VFS buffer / VDBE register allocation) and any context layer mentioning memory. Quarantine records now also capture the full `error.chain()` so post-mortem triage shows the actual subsystem instead of the bare "out of memory" line.
- **Active-source filter now has an mtime fallback that catches macOS append-mode writes.** `ActiveSessionSourceFilter::active_writer_reason` (`src/indexer/mod.rs`) consults `metadata.modified()` *first*, before the lsof-fd and advisory-lock probes. If a session JSONL was modified inside `CASS_ACTIVE_SESSION_RECENT_WRITE_WINDOW_SECS` (default `120s`), the filter skips it. Claude Code's macOS writer holds no writable fd that `lsof` advertises and takes no advisory lock, so the previous filter never fired and mid-write JSONLs were ingested, failed mid-parse, and got quarantined.

Both fixes ship together by necessity: tightening OOM detection without the mtime fallback pushes mid-write parse errors back onto the v0.4.7 (#250) hard-error / exit-9 crash-loop path that was the original quarantine system's reason to exist.

## [v0.5.1] -- 2026-05-21

**Watch-mode reliability: no more silent crash-loops, redundant salvage re-scans, or unrecoverable rebuild loops.**

Fixes for three field-reported `cass index --watch` failures on large archives:

- **Silent code-9 exits are gone (#250).** Index failures were logged at `debug!`
  (hidden by default), so a failing watch cycle exited with code 9 and left
  nothing in the log but a `drop_close` warning. Index failures now log at
  **ERROR** with the exit code and full message, and swallowed watch-cycle
  failures log the full error chain plus a "since_ts not advanced this cycle"
  note that explains the frozen-watermark / backlog-re-scan loop.
- **Historical salvage no longer re-scans fully-imported backups (#247).** A
  bundle whose progress checkpoint already covered the backup's entire
  conversation row-id space (daemon OOM-killed before the completion ledger
  marker landed) was re-scanned O(n) on every cycle — 5-12 min per batch with
  `imported=0`. It is now detected via the checkpoint, ledgered, and skipped.
- **Sparse-index rebuilds self-heal instead of looping after OOM (#248).** When
  the live lexical index reads sparse, cass repairs it from the canonical SQLite
  (the source of truth) per the Search Asset Contract; combined with staged-shard
  memory throttling, the repair completes within the host memory budget instead
  of OOM-looping. A completed checkpoint that disagrees with a sparse live index
  now emits a diagnostic warning.

## [v0.5.0] -- 2026-05-20

**`cass index` now protects the canonical SQLite archive instead of silently replacing it.**

A `--full` rebuild that detected an unhealthy current-schema canonical database
used to back it up and start over from an empty archive. Because the cass
archive can be the only surviving copy of conversations whose original agent
logs were pruned, retired, or truncated (see *Remote Archive Safety* in the
README), a transient health blip — a connection dropped mid-transaction, an OOM,
or a lock — could trigger an automatic wipe-and-start-empty of the source of
truth, buried in a `.bak` the operator never knew was created. This release
makes that case fail loudly and route through the archive-first `cass doctor`
recovery model instead. Derived lexical (Tantivy) and semantic (vector) indexes
continue to self-heal and rebuild from SQLite exactly as before — only the
canonical source-of-truth archive is now off-limits to automatic replacement.

This is the behavior change that warrants the minor version bump: the muscle
memory of "`cass index --full --force-rebuild` fixes everything" now stops one
step earlier and hands corruption recovery to `cass doctor`.

### Changed

- **`cass index --full` no longer auto-replaces an unhealthy canonical archive.**
  When a full rebuild detects an unhealthy current-schema database, indexing now
  stops with exit code 5 (`kind: storage`, non-retryable) and a message routing
  operators to `cass doctor check --json` for a read-only diagnosis, then a
  doctor repair plan or explicit backup restore — instead of backing the archive
  up and starting empty. `--force-rebuild` is explicitly no longer a
  corruption-repair backdoor. Removes `reopen_fresh_storage_for_full_rebuild`.
- **Orphan foreign-key self-heal (cass#202) is now blocking for the run.** A
  failed orphan-FK sweep previously logged a warning and continued; it now aborts
  the index run before any further writes (exit code 5, retryable after freeing
  memory/disk), because a connection dropped mid-transaction can OOM-poison every
  subsequent run.
- Exit-code-5 guidance in the README and `docs/LIMITS.md` now points at
  `cass doctor check --json` and archive repair/restore rather than
  `cass index --full --force-rebuild`.

### Added

- **Pre-index disk-headroom check.** Indexing refuses to start when the
  filesystem holding the cass data directory has less free space than required
  (default floor 512 MB, scaled to archive size), so SQLite, WAL, and
  lexical-scratch writes cannot fail mid-commit (exit code 14, retryable; bypass
  with `CASS_INDEX_SKIP_DISK_HEADROOM_CHECK=1`).
- **TUI swarm cockpit** seeds on `SwarmEntered` and now renders explicit empty
  and evidence-gap states ([f6568f73](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f6568f73)).

### Fixed

- **index**: account for streaming-producer memory before flush; preserve ingest
  on active and OOM-affected sources; quarantine non-watch poison sessions so a
  single bad session no longer stalls a run.
- **search**: auto-cap Tantivy writer threads by available memory; stop the
  search dispatcher from forcing JSON output when `--display` is set
  ([#245](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/245)).
- **storage**: avoid fragile frankensqlite query shapes during indexing;
  paginated bulk orphan-FK deletion replaces per-row deletes.
- **cli/daemon/models**: dispatch `cass daemon` in both routing branches; wire
  reranker model install.

### Internal

- **ci**: honor governed rebuild worker counts in tests; coverage-aware perf
  tests with lower Tantivy thread bounds; hardened SSH e2e and analytics
  guardrails; install UBS via its upstream `install.sh`.
- **docs**: corrected the `cargo install` invocation (the crate is
  `coding-agent-search`); backfilled the v0.4.8 changelog entry.

## [v0.4.8] -- 2026-05-16

**Stop chained orphan-sidecar growth from frankensqlite Windows VFS lock files.**

A long-running cass install on Windows NTFS could accumulate ~789k orphan files
(195 GB observed) under the data directory. The shape was `*-lock-pending`,
`*-lock-pending-lock-pending`, `*-lock-pending-lock-pending-lock-pending`, …
growing without bound across reindex sessions. Root cause was a two-way
interaction: frankensqlite's Windows VFS leaked three advisory-lock sidecars
per transient DB file (e.g. each `VACUUM INTO` backup), and cass's
`historical_bundle_root_paths` re-enumerated those orphan sidecars as
"backup roots" and reopened them — which spawned a fresh set of sidecars on
top of the orphan path, chaining indefinitely.

This release closes the cass side. The frankensqlite side (the actual leak
fix in `Vfs::delete`) ships in frankensqlite `fsqlite-vfs` 0.1.5 / commit
[`64363595`](https://github.com/Dicklesworthstone/frankensqlite/commit/64363595);
cass continues to pin frankensqlite by git rev, so picking up the full fix
on the cass side will require a follow-up rev-bump.

### Fixed

- **storage**: `historical_bundle_root_paths` now skips frankensqlite's three
  Windows advisory-lock sidecars (`-lock-shared`, `-lock-reserved`,
  `-lock-pending`) alongside the standard `-wal`/`-shm` filter, so an orphan
  sidecar can no longer be misidentified as a backup root and reopened.
  Reported by @oysteinkrog with a real 789k-file production reproduction; closed
  via #236 with an independent reimplementation
  ([commit](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/37b42058)).
  `is_backup_root_name` is intentionally left untouched so the existing
  `cleanup_old_backups` rotation continues to reap any pre-existing orphan
  lock sidecars already on disk in the field.

### Internal

Bug-fix rollup since v0.4.7 (none of these are direct user-facing fixes but
they shipped on this tag):

- **indexer**: surface disabled-connector exclusions; preserve watermarks
  with scan exclusions; unblock large streaming source scans.
- **installer**: validate Windows zip layout before extraction; prefer
  canonical Windows binary name; install exe basename on Windows POSIX
  shells; support legacy Windows exe in POSIX installer; avoid eager zip
  entry type binding; reject exe payloads off Windows; reject non-exe
  payloads on Windows; prefer platform binary after extraction; prefer
  legacy exe on Windows POSIX shells.
- **fts**: stream-based rebuild + pass-15/16/17 doctor `cleanup_apply` work.
- **ci**: gate UBS on changed-file regressions; release workflow inspects
  all matching releases before publish.

## [v0.4.7] -- 2026-05-14

**Registry and source-release alignment for the v0.4.6 publication fix.**

The `v0.4.6` tag remains immutable, but crates.io publication required one
more dependency and build-script correction after that tag. This patch release
cuts the registry-ready source from the matching commit instead of mixing
post-tag source into the `v0.4.6` release line.

### Fixed

- **crates.io installation**: publish against `franken-agent-detection` 0.1.7
  so the enabled `chatgpt` connector feature resolves from crates.io instead of
  depending on an unpublished registry feature set.
- **registry package verification**: allow Cargo's packaged manifest rewrite for
  git dependencies with explicit registry versions while keeping local
  path/git dependency contract checks strict during normal development.
- **fresh source reproducibility**: replace wildcard dependency constraints with
  the current resolved minimums and include the required source files in the
  package manifest so `cargo install coding-agent-search --version 0.4.7
  --locked` has a stable registry surface.

## [v0.4.6] -- 2026-05-14

**Windows release-build fix for the v0.4.5 publication attempt.**

The `v0.4.5` tag remains immutable, but local fallback release builds uncovered
that CASS's direct vendored OpenSSL dependency was Unix release packaging glue,
not application logic. Keeping it active on Windows forced MSVC cross-builds
through OpenSSL's `VC-WIN64A` source build and blocked a real PE artifact.

### Fixed

- **Windows MSVC release builds**: scope the direct vendored OpenSSL dependency
  to non-Windows targets while preserving static OpenSSL linking for Unix
  release binaries.

## [v0.4.5] -- 2026-05-13

**Release-integrity fix for the v0.4.4 publication attempt.**

The `v0.4.4` tag remains immutable, but `main` received one more doctor cleanup
fix before the binary fallback build completed. This patch release keeps the
macOS CoreML link fix from v0.4.4 and publishes artifacts from the matching
post-tag commit instead of mixing untagged code into v0.4.4 assets.

### Fixed

- **Doctor cleanup journaling diagnostics**: cleanup-apply now logs structured
  warnings when `RunStarted` or `RunEnded` journal appends fail, so operators can
  see why crash recovery would otherwise classify a run as malformed or
  indefinitely in-flight.

## [v0.4.4] -- 2026-05-13

**Release-publication fix for the v0.4.3 stability work.**

The `v0.4.3` tag was already pushed before the macOS release builder exposed a
native link failure in the ONNX Runtime static archive. This patch release keeps
the v0.4.3 issue fixes intact and adds the missing macOS CoreML framework link
hint so Apple Silicon release artifacts can be built without rewriting the
already-pushed tag.

### Fixed

- **Apple Silicon release builds**: emit a macOS-only `CoreML` framework link
  hint from `build.rs` because the aarch64 ONNX Runtime static archive used by
  `ort-sys` references CoreML classes while `ort-sys` currently emits only
  Foundation.

## [v0.4.3] -- 2026-05-13

**Stability release for the v0.4.2 indexing, doctor, and connector reports.**

This release resolves the recent open GitHub issue cluster around watch indexing,
Codex/OpenCode ingest, doctor recovery, raw-mirror retention, and schema drift.

### Added

- **Raw-mirror retention tooling**: `cass mirror prune` now provides explicit
  dry-run/apply plans, `--keep-tag` pinning, a 7-day safety hold-down, audit
  logging, and doctor/stat surfaces for raw-mirror storage growth
  ([#221](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/221)).
- **Codex ingest trace mode**: `cass index --json --robot-trace-ingest` emits
  per-batch NDJSON with `batch_n`, `batch_msgs`, `wall_ms`,
  `lookups_against_global`, and detailed duplicate-lookup counters for future
  performance bisects ([#228](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/228)).
- Archive-first doctor documentation: the recovery runbook and README now
  describe the doctor v2 command suite, candidate-based repair flow,
  fingerprinted restore/cleanup/archive export workflows, source-pruning and
  sole-copy warnings, support-bundle handoff, and the rule that cass preserves
  archive evidence before attempting repair.

### Changed

- **OpenCode support**: cass now pins `franken-agent-detection` to a revision
  that understands the current Drizzle-backed `opencode.db` schema
  ([#227](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/227)).
- **Claude discovery resilience**: the pinned detector now keeps
  `$HOME/.claude/projects` as a fallback when `XDG_CONFIG_HOME` is set, so
  isolated automation profiles do not accidentally hide existing Claude Code
  histories.
- **Dependency refresh**: routine library updates include `lru 0.18`,
  `fastembed 5.13.4`, `pbkdf2 0.13`, `wide 1.4`, `assert_cmd 2.2.2`,
  `blake3 1.8.5`, `clap_complete 4.6.5`, and the latest compatible OpenSSL
  crate line.
- Doctor migration guidance now treats historical cass archives, raw-session
  mirrors, backup bundles, receipts, and source ledgers as preservation targets.
  Existing data dirs migrate additively; derived assets can be rebuilt through
  planned doctor workflows, but recovery recipes should not instruct users to
  hand-remove archive paths or provider session logs.

### Fixed

- **Watcher OOM progress**: watch ingest now processes bounded chunks, splits
  out-of-memory batches recursively, quarantines irreducible oversized records,
  and still advances the high-water mark after partial success
  ([#218](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/218)).
- **Token-column drift**: schema repair restores missing `conversations`
  token-total columns so upgraded/downgraded archives can resume ingest
  ([#222](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/222)).
- **Duplicate message/index invariants**: batched persistence refreshes partial
  pending lookups, keeps duplicate `(conversation_id, idx)` handling aligned
  with SQL uniqueness, and raises stale-low lexical shard footprints before
  rebuild planning
  ([#212](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/212),
  [#226](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/226)).
- **Post-rebuild incremental stalls**: the streaming byte limiter no longer
  loses wakeups during repeated shrink/grow controller updates
  ([#213](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/213)).
- **Doctor/search hints**: removed stale `--mode lexical` repair hints from
  status/doctor guidance where default hybrid fail-open behavior is the correct
  operator path.

### Testing

- Added regression coverage for watch OOM splitting, schema repair, duplicate
  message merging, shard-footprint planning, byte-limiter wakeups, raw-mirror
  pruning/doctor warnings, OpenCode Drizzle ingest, and Codex ingest tracing.
- Regenerated robot JSON and robot-doc goldens for the new CLI/docs contract.

## [v0.3.7] -- 2026-04-23

**Indexer stall observability + zero-writer deadlock fix.**

Cuts a release from current `main` so reporters hitting the [#196](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/196) "phase:indexing, current:0/N indefinitely" shape get the new forensic machinery and a credible root-cause candidate.

### Added

- **Stall-detection watchdog for `cass index --json`.** Emits a one-shot `stall_detected` NDJSON event when no forward progress has been observed for `CASS_INDEX_STALL_DETECT_SECS` (default 120s; `0` disables). The event carries an on-disk snapshot — lexical rebuild checkpoint (parsed when ≤64 KiB), Tantivy segment count/bytes, and the index-run lock file — plus a hint with strace/gdb/`/proc/<pid>/stack` snippets so operators hitting the hang can attach a live stack to the issue. Latched once per phase, reset on phase transitions, never cancels the run ([`ae411287`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ae411287)).

### Fixed

- **Zero-writer connection-manager deadlock.** `FrankenConnectionManager::new` now clamps `max_writers < 1` up to 1 and pre-fills the bounded writer-token channel accordingly. Previously, opening a connection manager with `max_writers: 0` left the writer-token channel empty, so the first writer acquisition blocked forever against an empty semaphore. Candidate root cause for [#196](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/196) ([`fd3196fb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fd3196fb)).
- **Indexer lexical manifest**: crash-safe rebuild manifest publish + propagate persistence failures instead of swallowing them ([`6decefa8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6decefa8)); sharded rebuild also persists the equivalence ledger and generation manifest ([`75262206`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/75262206)).
- **Semantic backfill**: warn on NULL `created_at` during semantic backfill instead of silent drift ([`ff156d29`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ff156d29)).
- **Search pre-cache**: propagate pre-cache reload failures instead of silently continuing ([`7ec6163f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7ec6163f)).
- **CLI robot output**: `--aggregate` rejects unknown fields as a usage error instead of silently dropping them ([`d3e8dc31`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d3e8dc31)); `--dry-run` robot output stays reproducible ([`e068eb83`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e068eb83)).

### Testing

- New proptest coverage for indexer memoization serde round-trips and quarantine summaries ([`a5522d71`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a5522d71)).
- Pinned WAL-compaction ordering for small-final resume publish ([`dc0dd881`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dc0dd881)).
- Connector and query parser fuzz harnesses added ([`d698f59a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d698f59a)).

---

## [v0.3.0](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.3.0) -- 2026-04-12

**GitHub Release** with downloadable binaries.

This release line focuses on semantic-search concurrency safety, legacy-data correctness, new resume ergonomics, and making the release pipeline fail early instead of cutting broken or partially-updated releases.

### Search and concurrency

- **Semantic search deadlock / TOCTOU hardening**: reduce semantic-search lock scope, add context-token validation across lazy loaders, make two-tier cache availability mode-aware, and add regression coverage for stale-context and cache-poisoning races
- **Retry storm mitigation**: replace deterministic SQLite retry sleeps with shared jittered exponential backoff for `Busy`, `BusySnapshot`, and related write-conflict paths
- **Stale lock recovery**: reap dead-owner `index-run.lock` metadata on read so stale lock files stop wedging search and health flows
- **Query correctness**: NFC-normalize queries and harden empty-index health/status detection

### CLI, data quality, and indexing

- **`cass resume`**: add a CLI subcommand that resolves a session path into a ready-to-run harness resume command, then harden it with UUID validation, false-positive guards, and structured diagnostics
- **Legacy NULL-agent correctness**: fix search, UI, export, stats, context loading, salvage, and related-session paths that previously dropped or crashed on rows with `NULL agent_id`
- **Indexer / FTS rebuild reliability**: fix large-batch OOMs, zero-row batch aborts, repeated full-rebuild loops, and several frankensqlite materialization-heavy query paths

### Release engineering

- **Release workflow hardening**: require `HOMEBREW_TAP_TOKEN` before cutting a release, clone all sibling path dependencies in every release job, and avoid failing post-release on a missing Homebrew dispatch token
- **Installer fallback**: stop probing for a non-existent Intel macOS prebuilt and fall back cleanly to source builds instead
- **Crates publish readiness gate**: validate `cargo package` before attempting `cargo publish` so the workflow warns and skips instead of failing when the current dependency graph is not registry-ready

---

## [v0.2.7](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.7) -- 2026-04-05

**GitHub Release** with downloadable binaries.

Validation release focused on proving the 0.2.6 database/indexing repairs hold under end-to-end conditions and shipping the new regression coverage as part of a fully green release gate.

### Test coverage

- **Duplicate `fts_messages` migration repair, end to end**: add a full CLI regression that injects the legacy duplicate-schema corruption, proves stock SQLite clients fail, runs `cass index` to repair the database, and then verifies health, FTS readability, and post-repair incremental indexing/search behavior
- **Remote `source_id` FK safety across both persistence paths**: add detailed serial and `BEGIN CONCURRENT` regressions that prove unknown non-`local` sources are auto-registered exactly once, preserve provenance, and keep `foreign_key_check` clean
- **Incremental watch/index stability after `autocommit_retain` shutdown**: add a repeated idle `watch --watch-once` regression that verifies `autocommit_retain` is actually disabled, idle cycles stay healthy, and newly appended content is still ingested correctly

### Release engineering

- Re-run the full release gate through `rch`, including `cargo fmt --check`, full `cargo test`, `cargo check --all-targets`, and `cargo clippy --all-targets -- -D warnings`
- Harden the remote test gate by moving `TMPDIR` and `CARGO_TARGET_DIR` off `tmpfs` for the full-suite run so release validation is not derailed by worker RAM-disk exhaustion during link steps

---

## [v0.2.6](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.6) -- 2026-04-03

**GitHub Release** with downloadable binaries.

Stability release focused on hard database failures in the 0.2.5 upgrade path, incremental indexing reliability, and getting the full test suite back to green.

### Bug fixes

- **V14 FTS migration repair**: Fix duplicate `fts_messages` schema rows left behind by older upgrade paths and harden the frankensqlite-owned rebuild/recovery flow so upgraded databases remain readable instead of tripping `malformed database schema (fts_messages)` on open
- **Incremental source FK guard**: Register non-`local` `source_id` values during batched incremental persistence so watcher-driven indexing no longer crash-loops on `FOREIGN KEY constraint failed`
- **Incremental writer memory stability**: Disable `autocommit_retain` on supported frankensqlite connections and tighten writer lifecycle behavior to stop the watch/index incremental path from retaining unbounded MVCC snapshots and running out of memory
- **Readonly/maintenance-state regressions**: Scope maintenance locks to the active database, prefer heartbeat timestamps in fallback metadata, and fix several readonly/write-path regressions that were cascading through UI, export, and search tests
- **Watch/index correctness**: Harden watch-once semantics, checkpoint refresh behavior, and fixture validity so incremental indexing matches the intended runtime contract

### Test and release engineering

- Reconcile outdated integration expectations with current frankensqlite behavior, including storage migration, watch E2E, pages/search, and robot-mode coverage
- Fix doctests, clippy regressions, and non-test utility bins so `cargo test`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` all pass cleanly before release

---

## [v0.2.5](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.5) -- 2026-03-28

**GitHub Release** with downloadable binaries.

Hot-fix release addressing FTS5 regression and release infrastructure issues from v0.2.4.

### Bug fixes

- **FTS5 shadow-table corruption**: Close frankensqlite handle before rusqlite FTS schema mutation to prevent shadow-table corruption ([`fb7f431`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fb7f4311))
- **FTS cleanup robustness**: Replace `writable_schema` FTS cleanup with `DROP TABLE` + add duplicate schema regression test ([`437758e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/437758e9))
- **Watch-once mtime watermark bypass**: Force `since_ts=None` in watch-once mode so old messages are found regardless of mtime watermarks ([`f66ce17`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f66ce17e))
- **Install checksum fallback**: Add `SHA256SUMS` (no `.txt` extension) as checksum fallback for installer verification ([`4aaa07e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4aaa07e4))
- **v0.2.4 Linux x86_64 binary was aarch64** (issue [#140](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/140)): Release workflow now adds a post-build architecture verification step to prevent cross-architecture packaging errors

### Refactoring

- **Unified DB engine**: Remove rusqlite FTS dual-backend; make frankensqlite sole DB engine with targeted watch-once fast path and local source scanning ([`a0aa6f6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a0aa6f63))

### Performance

- **Bulk import optimization**: Defer WAL checkpoints and Tantivy updates during bulk imports; add fast schema probe to bypass recovery path ([`8a1c0e0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8a1c0e04))

### Scripts

- **Resumable watch-once batch driver**: Add resumable watch-once batch driver for large session tree reconciliation ([`ca94cd2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ca94cd23))
- **Memory-aware autotuning**: Add memory-aware autotuning and per-root state isolation to watch-once batch driver ([`65c3fad`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/65c3fadc))

---

## [v0.2.4](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.4) -- 2026-03-27

**GitHub Release** with downloadable binaries.

### Bug fixes

- **INSERT...SELECT UPSERT/RETURNING fallback** (#134): Convert multi-row `INSERT OR IGNORE` to row-wise execution for frankensqlite compatibility ([`f4e1452`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f4e1452e))
- **Cross-database rowid watermark**: Remove invalid cross-database rowid comparison; force autoindex on message fetches ([`f4424ee`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f4424ee9))
- **Auto-repair missing analytics tables** when schema version markers lie ([`8d36a04`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8d36a04c))
- **FrankenStorage connection handling**: Explicitly close all connections instead of relying on Drop ([`7f2a589`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7f2a5899), [`92a4173`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/92a41737))
- Include `extra_json` in conversation character count ([`d744ea7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d744ea78))
- Suppress frankensqlite internal telemetry in default log filter ([`b4bde82`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b4bde82c))
- Drop and recreate FTS on full reset; batch historical imports with queryable-first sort ([`06564e6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/06564e63))

### New features

- **Historical session recovery toolkit**: Recover sessions from historical bundles ([`548d50b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/548d50b9))
- **Database health integration**: quick_check, FTS consistency repair, historical bundle watermark probing ([`4c91ad3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4c91ad30))
- **Crush connector**: Integrate Crush connector from franken_agent_detection ([`dfe9cff`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dfe9cffa))
- **Resumable lexical rebuild**: Durable checkpoints for lexical rebuild and historical salvage ([`d192703`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d192703f))
- **Seed canonical DB** from best historical bundle via VACUUM INTO ([`d4e7126`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d4e7126c))

### Performance

- Replace `COUNT(*)` rebuild fingerprint with fs stat; lightweight conversation projection ([`cec08ac`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cec08ac4))
- Batch message fetching and multi-threshold commit triggers for lexical rebuild ([`bc48c67`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bc48c670))
- Restructure daily stats rebuild to co-locate message scanning with conversation batches ([`7959d04`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7959d04b))

---

## [v0.2.3](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.3) -- 2026-03-24

**GitHub Release** with downloadable binaries.

Incremental reliability release covering streaming, indexing, and UI fixes since v0.2.2.

### Search and indexing

- **FTS5 contentless mode (schema V14)**: Full-text search tables migrated to contentless mode, reducing DB size while preserving query performance ([`5a30465`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5a304657))
- **LRU embedding cache**: Progressive search caches ONNX embeddings in an LRU to avoid redundant inference ([`a8f7a52`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a8f7a522))
- **Expanded query pipeline**: Major search query expansion with improved progressive search integration, phase coordination, and daemon worker simplification ([`d937265`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d9372655), [`c590ccd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c590ccd8), [`bd9ab48`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bd9ab484))
- **NaN-safe score normalization**: Prevent NaN from propagating through blended scoring paths ([`1eb68a9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1eb68aa9))
- **Penalize unrefined documents**: Two-tier blended scoring now down-ranks documents that were never refined ([`b0c612c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b0c612cd))
- **Parallel indexing**: Indexer processes multiple connector sources concurrently ([`40627d2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/40627d25))

### TUI and user interface

- **HTML/PDF export pipeline rewrite**: Complete overhaul of export rendering with improved layout and PDF support ([`98757e6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/98757e67))
- **TUI search overhaul**: Redesigned search interaction with improved result rendering ([`40627d2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/40627d25))
- **Analytics dashboard expansion**: Additional chart types, structured error tracking, and improved layout ([`b393593`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b3935935), [`f073b99`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f073b994))
- **Click-to-position cursor**: Click anywhere in the search bar to place the cursor, with pane-aware hover tracking ([`69d2518`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/69d25182))
- **UltraWide breakpoint**: New layout breakpoint for ultra-wide terminals with style system refactoring ([`baf3310`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/baf33104))
- **Sparkline bar chart in empty-state dashboard** ([`3fb1c44`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3fb1c447))
- **Footer HUD lanes**: Conditional footer HUD with compact formatting and refined empty-state display ([`bf314fb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bf314fba))
- **Search-as-you-type supersedes in-flight**: New queries cancel stale in-flight requests ([`e163926`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e163926c))
- **Alt+? help toggle** and consistent dot-separator detail metadata ([`a293bce`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a293bce4))

### Health and storage

- **WAL corruption detection**: Degraded health state reported when WAL corruption is detected ([`a738a9b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a738a9b0))
- **Pages subsystem expansion**: Config input, encryption, and export improvements ([`426d6fe`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/426d6fe5))

### Export

- **Skill injection stripping**: Proprietary skill content is stripped from HTML, Markdown, text, and JSON exports ([`dd568dc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dd568dc8), [`e1886a0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e1886a0e))
- **Accurate message-type breakdown** in HTML export metadata ([`8b81ed7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b81ed77))
- **Legible code blocks without CDN dependencies** ([`3f690e9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3f690e91))

### Dependency migration

- **Rusqlite to frankensqlite**: Complete migration of remaining `src/` and test files ([`e372307`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e3723076), [`232bdd1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/232bdd16))
- **Reqwest removal**: HTTP calls migrated to asupersync; reqwest eliminated from the dependency tree ([`80d9885`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/80d98854), [`dc90e9f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dc90e9f7))

### Bug fixes

- **Watch mode**: Replace `thread::sleep` throttle with `recv_timeout` cooldown to prevent event loss ([`89c78cf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/89c78cf0))
- **Watch mode**: Add `--watch-interval` throttle to prevent CPU burn ([`40f35f8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/40f35f8f))
- **Backup cleanup**: Skip directories and WAL/SHM sidecars; tighten retention assertion ([`a5c9e75`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a5c9e756), [`2ad0bf6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2ad0bf66))
- **Windows**: Safe atomic file replacement for config and sync state ([`9353938`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/93539383))
- **XSS prevention** in simple HTML export and defensive string slicing ([`4fcc026`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4fcc026e))
- **UTF-8 panic** in `smart_truncate` and silent rowid failures fixed ([`c874303`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c8743037))
- **Display-width correctness**: `shorten_label` and dashboard truncation use `display_width` instead of `chars().count()` ([`7d89643`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7d896438), [`76d8671`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/76d86714))
- Zero compiler warnings achieved ([`3c83c68`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3c83c680))

---

## [v0.2.2](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.2) -- 2026-03-15

**GitHub Release** with downloadable binaries.

### Security

- **Secret redaction**: Secrets detected in tool-result content are redacted before DB insert ([`eb9444d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/eb9444d0))

### Storage and database

- **FTS5 on FrankenSQLite**: Register FTS5 virtual table on frankensqlite search connections; fix doctor diagnostics ([`f3acfec`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f3acfecb), [`0773593`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/0773593c))
- **Doctor improvements**: Chunked FTS rebuild to prevent OOM; ROLLBACK on failed rebuild; correct SQL ([`3e736ab`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3e736ab4), [`afad4e9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/afad4e9a), [`75e2008`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/75e20085))
- Replace `sqlite_master` queries with direct table probes ([`892d1bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/892d1bd0))

### Safety and reliability

- Replace unwrap calls with safe error handling across search, export, timeline, and tests ([`300caa4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/300caa4b), [`900abdf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/900abdfa))
- Null-safety guards in router, service worker, and perf tests ([`c5f64c3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c5f64c35))

### UI

- **Colorblind theme redesign**: Palette redesigned for deuteranopia/protanopia; fix preset cycling ([`6807be3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6807be3f))
- Missing-subcommand hints for the CLI ([`c0cf17a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c0cf17a3))

### Export

- Load sessions from DB instead of JSONL; optimize rendering ([`3338ac3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3338ac38))

### Bug fixes

- Correct stale detection grace period and redact JSON keys ([`cf5fc17`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cf5fc17c))
- Eliminate daemon connection cloning; handle requests concurrently ([`87e8b3d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/87e8b3df))
- Fix hash encoding, memory tracking, score fallback, and SSH keepalive ([`bab8953`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bab89538))
- Harden pages decrypt, preview server, and exclusion API ([`827ece2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/827ece29))

---

## [v0.2.1](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.1) -- 2026-03-09

**GitHub Release** with downloadable binaries.

### Connectors

- **Kimi Code and Qwen Code** re-export stubs added ([`886af59`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/886af59e))
- **Copilot CLI** connector module ([`e87d6f1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e87d6f18))

### Semantic search

- **Incremental embedding in watch mode**: Semantic index updates as new sessions arrive ([`d746f99`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d746f993))

### Accessibility

- **Colorblind theme preset** for deuteranopia/protanopia ([`0133256`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/01332563))

### Release infrastructure

- Statically link OpenSSL to eliminate `libssl.so.3` runtime dependency ([`efe5d32`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/efe5d321))
- Lower ARM64 glibc floor to 2.35 ([`074a678`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/074a6781))
- Use ubuntu-24.04 runners for Linux release builds ([`050db98`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/050db985))

### Bug fixes

- Make TUI resize evidence logging opt-in to prevent disk exhaustion ([`c343ac9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c343ac92))
- Consume Enter and navigation keys in export modal ([`fc2b3d6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fc2b3d67))
- Include "tool" role messages in all export formats ([`e32ee69`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e32ee693))
- `health --json` now reports real DB stats; expand skips non-message records ([`6ce238b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6ce238b9))
- Fix Scoop manifest URL and PowerShell checksum verification ([`7bd3a02`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7bd3a028))
- Fix installer temp path for Windows provider-neutrality ([`d4b5b5e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d4b5b5eb))

---

## [v0.2.0](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0) -- 2026-03-02

**GitHub Release** with downloadable binaries. Major milestone: complete migration from `rusqlite` to `frankensqlite`.

### FrankenSQLite migration (headline change)

- Full replacement of rusqlite with frankensqlite across all modules: storage, pages, analytics, bookmarks, secret scan, summary, wizard, and lib.rs ([`e5789a7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e5789a7f), [`39d3bb0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/39d3bb01), [`6657c98`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6657c980), [`89c1a0f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/89c1a0fb))
- `FrankenStorage` type alias replaces `SqliteStorage`; `fparams!` macro replaces `params!`; `BEGIN CONCURRENT` transaction support ([`e5789a7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e5789a7f), [`51cf9d5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/51cf9d54))
- Full V13 schema, transaction support, and compatibility gates ([`e5789a7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e5789a7f))
- Path dependencies converted to git dependencies for release ([`81f2560`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/81f25604))

### Search

- **Two-tier progressive search**: Combines fast lexical search with semantic refinement ([`653836f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/653836fb))
- Robust empty-index handling and dynamic SSH probe paths for `TwoTierIndex` ([`2b6d8a6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2b6d8a67))
- Normalize embedding scores in two-tier search refinement ([`ee3b1ce`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ee3b1ce5))
- Bypass BM25 for empty queries; show date-sorted results instead ([`d1c4627`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d1c46277))

### Connectors

- **Pi-Agent**: Recursively index nested Pi-Agent session subdirectories ([`4990fdf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4990fdfc))

### Export

- **Export tab**: HTML/Markdown export keybindings added to TUI ([`98863d3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/98863d39))
- Load conversations from indexed database with illustration ([`1502b29`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1502b295))
- Prevent silent file overwrites with no-clobber retry (up to 1024 collisions) ([`c4dfde7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c4dfde78))
- Eliminate export path/status race in detail modal ([`1579a08`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1579a08a))

### TUI

- **Workspace filtering**, WCAG theme fixes, and daemon hardening ([`690506f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/690506f0))
- Real-time indexer progress bar, help popup scrollbar ([`71d779b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/71d779be))
- Word-jump navigation, richer empty states, Unicode display fixes ([`e37b817`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e37b8176))
- Download progress clamped to 100% ([`5180a5d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5180a5dd))

### Bug fixes

- Runtime AVX CPU check with clear error message ([`e0dfc91`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e0dfc918))
- Handle `limit=0` (no limit) in cursor pagination ([`2232ec0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2232ec00))
- Case-insensitive comparison for agent detection from paths ([`c1a18b3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c1a18b3e))
- Handle nullable workspace field in SQLite search results ([`e720b3b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e720b3bd))
- Replace `softprops/action-gh-release` with `gh` CLI to fix missing releases ([`ff74417`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ff74417a))
- Fallback to message timestamps when conversation start time is missing ([`e0d1232`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e0d12325))
- Prevent stale raw event replay in TUI ([`044bda5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/044bda50))

---

## [v0.1.64](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.64) -- 2026-02-01

**GitHub Release** with downloadable binaries (re-created after the `softprops/action-gh-release` draft bug).

### Connectors

- **ClawdBot** connector for ClawdBot coding-agent sessions ([`4744ff5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4744ff51))
- **Vibe** connector for Vibe coding sessions ([`38d44bb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/38d44bb9))
- **ChatGPT web export** import command ([`002f12c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/002f12c8))

### HTML export redesign

- Message grouping with tool badge overflow rendering ([`aee1701`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aee17014))
- Tool badge popover JavaScript for inline tool-call inspection ([`e9e8ad6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e9e8ad6f))
- Search-hit message glow highlighting ([`86966bb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/86966bb4))
- Upgraded typography, popover positioning, CSS fallbacks ([`ace08db`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ace08db1))

### Deployment

- **Cloudflare Pages**: Direct API upload with CLI flags for deployment configuration ([`7776fe8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7776fe86), [`48e02db`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/48e02db9))

### Search

- **Two-tier progressive search** introduced ([`653836f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/653836fb))
- Reranker registry with bake-off eligible models ([`34a3545`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/34a3545c), [`4ea6110`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4ea6110a))
- Embedder registry for model bake-off ([`809ba65`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/809ba658))

### Infrastructure

- **LazyDb**: Deferred SQLite connection for faster TUI startup ([`03e17b4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/03e17b49))
- **Stale detection system** for watch daemon ([`320b8bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/320b8bdf))
- Daemon module gated behind `#[cfg(unix)]` for Windows compatibility ([`3f51c76`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3f51c764))
- Doctor: detect and recreate missing FTS search table ([`6b1541f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6b1541fe))
- Switched from Rust nightly to stable toolchain ([`5983515`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/59835155))
- Bake-off evaluation framework with `EMBEDDER` env var for semantic index ([`260da55`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/260da553), [`125a8b6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/125a8b62))

### Bug fixes

- Deterministic sort order with `total_cmp` and index tie-breaking ([`7d92b53`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7d92b53f))
- Harden arithmetic operations and sanitize socket path ([`81a055b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/81a055ba))
- Safe integer casts with `try_from` and hardened SQL LIKE escaping ([`743702a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/743702ac), [`32e0e70`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/32e0e704))
- Harden JS initialization and search/popover behavior in HTML export ([`5a24996`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5a249963))
- Bakeoff division-by-zero in latency calculation ([`df836fe`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/df836fed))

---

## v0.1.50 -- 2026-01-04 (tag only)

### Connectors

- **Factory (Droid)** connector and **Cursor v0.40+** support ([`85dd4cb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/85dd4cb1))

### Performance

- Batched transaction support with debug logging ([`97e1926`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/97e1926d))
- Centralized connector instantiation ([`9f264ad`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9f264ade))

### Bug fixes

- Windows double-keystroke, Codex export, and Amp connector issues ([`cc9250d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cc9250d1))
- Make Cursor `source_path` unique per conversation ([`0448767`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/04487672))

---

## v0.1.36 -- v0.1.48 (2025-12-17 to 2025-12-30, tags only)

Rapid-fire release cycle focused on CI/CD and cross-platform builds. Most tags in this range are single-commit CI fixes.

### Semantic search (v0.1.36)

- **Semantic search infrastructure**: Embedder trait, hash embedder, canonicalization, and HNSW index foundation ([`e75f20b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e75f20b0), [`e28c883`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e28c8832))
- **WSL Cursor support** and chained search filtering ([`322ffa4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/322ffa4c))
- **Roo Cline** and Cursor editor connector support ([`bf27e5d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bf27e5d3))

### Remote indexing (v0.1.36)

- Support for remote sources with improved scanning architecture ([`43ba1c1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/43ba1c18))
- Dynamic watch-path detection via `root_paths` in `DetectionResult` ([`605441f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/605441fe))

### Security (v0.1.36)

- Path traversal prevention in sources ([`25ce09d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/25ce09da))
- Markdown injection prevention in exported results ([`8832e92`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8832e926))

### CI/CD (v0.1.37 -- v0.1.48)

- ARM64 Linux builds via cross-compilation, then native ARM64 runner ([`812bdc3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/812bdc35), [`4ac30fe`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4ac30fee))
- Vendored OpenSSL for ARM64 ([`de83181`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/de83181099dd72f202bd9052691bebdcd6588015))
- Version-agnostic golden contract tests ([`27dca3d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/27dca3db))
- Base64 updated to 0.22 for API compatibility ([`3ccd419`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3ccd4196))

### Bug fixes

- Correct duration calculation for millisecond timestamps in timeline ([`322ffa4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/322ffa4c))
- DST ambiguity and gap handling in date parsing ([`cf3a8f2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cf3a8f2e))
- Proper shell quoting for SSH and auto-index after sync ([`e0a0f1f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e0a0f1fb))
- Phrase query semantics and tokenization improvements ([`c105489`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c1054891))
- TUI EDITOR parsing with arguments ([`c91207f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c91207f2))

---

## v0.1.35 -- 2025-12-02 (tag only)

### Connectors

- **Pi-Agent** connector for the pi-mono coding agent, with model tracking in the author field ([`b333597`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b3335970), [`a3cee41`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a3cee41a))

---

## v0.1.34 -- 2025-12-02 (tag only)

### CI/CD

- **Multi-platform release pipeline** with self-update installer support ([`23714de`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/23714de5))

### CLI

- `export`, `expand`, and `timeline` commands with syntax highlighting ([`9a70d22`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9a70d221))
- Parallel connector scanning with agent discovery feedback ([`1120ab1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1120ab19))

### Bug fixes

- UTF-8 safety improvements and UX refinements in TUI ([`6fe0b2f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6fe0b2fd))
- Tantivy index resilience and correctness improvements ([`b5a9ee3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b5a9ee3d))
- File-level filtering restricted to incremental indexing only ([`c55a299`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c55a299b))

---

## v0.1.32 -- 2025-12-02 (tag only)

### Connectors

- **Cursor IDE** and **ChatGPT desktop** connectors ([`546c054`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/546c054b))
- **Aider** connector support in watch mode ([`8b6dd69`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b6dd69f))
- Improved Aider chat file discovery ([`9c10901`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9c109011))

### CLI

- Search timeout, dry-run mode, and `context` command ([`634c656`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/634c656f))
- Agent-first CLI improvements for robot mode ([`b4965d3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b4965d3b))
- Fuzzy command recovery for mistyped subcommands ([`7fd1682`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7fd16824))

### TUI

- Sparkline visualization for indexing progress ([`9f4b69c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9f4b69c4))
- Larger snippets, better density, persistent per-agent colors ([`7819a49`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7819a49c))
- Ctrl+Enter queue and Ctrl+O open-all shortcuts ([`4b6d910`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4b6d9101))

### Performance

- Batch SQLite inserts in indexer ([`47eba1f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/47eba1f0))
- Replace blocking IO with `tokio::fs` in async update checker ([`37ad11f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/37ad11fd))

### Security

- Harden `open_in_browser` with URL validation ([`be7560b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/be7560bb))
- Replace dangerous unwrap calls in indexer with proper error handling ([`8215b23`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8215b23e))

### Bug fixes

- WCAG hint text contrast boost ([`ab52ec8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ab52ec82))
- Transaction wrapping and NULL handling for data integrity ([`9b20566`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9b20566e))
- Populate `line_number` from `msg_idx` in search results ([`8351f18`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8351f189))
- Versioned index path in status/diag commands ([`49c64c6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/49c64c68))
- Critical Aider `detect()` performance fix and Codex indexing fix ([`50568da`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/50568da0))

---

## v0.1.28 -- v0.1.31 (2025-11-30 to 2025-12-02, tags only)

### Search

- **Wildcard and fuzzy matching** in the query engine ([`f85f2a0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f85f2a0e))
- Implicit wildcard fallback for sparse results ([`ab83f03`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ab83f038))
- Explicit wildcard search support ([`c8e9c09`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c8e9c094))
- CLI introspection and refreshed search/index plumbing ([`9e63ba1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9e63ba1b))

### TUI

- **Detail pane and inline search** in a major TUI expansion ([`b0ffa28`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b0ffa28c))
- **Modular UI components** for enhanced TUI experience ([`e7d4875`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e7d4875e))
- **WCAG-compliant theme system** with accessibility support ([`42bf621`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/42bf6218))
- Centralized keyboard shortcuts in `shortcuts.rs` ([`ca0612b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ca0612bd))
- Breadcrumbs component and extracted time_parser module ([`9a5bce7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9a5bce79))

### Connectors

- **Aider** chat history connector ([`7c89f6d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7c89f6d5))

### CLI

- Pagination, token budget, and new robot commands ([`4427192`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4427192b))
- Alt modifier required for vim-style navigation shortcuts (no letter swallowing) ([`78639c6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/78639c6b))

### Export

- **Bookmarks and export functionality** with expanded public API ([`57127ac`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/57127aca))

---

## v0.1.22 -- v0.1.27 (2025-11-26 to 2025-11-28, tags only)

### Search

- **Schema v4**: Edge n-gram prefix fields and preview for instant prefix search ([`f77fc0e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f77fc0e4))
- **LRU prefix cache, bloom filter, warm worker**, and manual query builder ([`4d36852`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4d368525))
- Schema v2 with `created_at` field; deduplicate noisy hits; sanitize queries ([`5206b66`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5206b662))
- Search pagination offset and quiet flag for robot runs ([`96e2b25`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/96e2b259))

### TUI

- **Premium theme system overhaul** with Stripe-level aesthetics ([`4e6058e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4e6058e5))
- Progress display, markdown rendering, adaptive footer, Unicode safety ([`2983c1d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2983c1d))
- Atomic progress tracking for TUI integration ([`5fc77ee`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5fc77ee1))
- Richer detail modal parsing and updated hotkey/help coverage ([`448603a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/448603a5))
- Indexing status visibility improvements ([`f91ec31`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f91ec314))

### Connectors

- Fix message index assignment consistency across claude_code, codex, gemini ([`04ed880`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/04ed8809))
- Proper `since_ts` incremental filtering for all connectors ([`27e0ef8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/27e0ef88))
- Immediate Tantivy commit after each connector batch in watch mode ([`47f5a0f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/47f5a0f3))

### Bug fixes

- Fix snippet truncation for multibyte UTF-8 characters ([`cf26dcc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cf26dccd))
- Fix query history debouncing ([`290baac`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/290baaca))
- Read-only database access for TUI detail view ([`7e9118b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7e9118b2))
- Disable text wrapping in search bar for cursor visibility ([`ff80172`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ff801727))

---

## v0.1.19 -- v0.1.21 (2025-11-25, tags only)

### Connectors

- **Rewrite all connectors** to properly parse real agent data formats ([`e492d1b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e492d1b6))

### TUI

- **Major UX polish** (Sprint 5): Comprehensive UI improvements ([`b5242f0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b5242f0f))

### CLI

- Force rebuild handling for the indexer ([`816e863`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/816e863302ea3f80d3b467b9f86e860f820044c8))

### Infrastructure

- Fix update loop by version bumps (v0.1.12, v0.1.19) ([`2d494c4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2d494c4b), [`35fecaf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/35fecafe))
- Fix binary name: configure `cass` in Cargo.toml ([`2aa5edf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2aa5edf1))

---

## v0.1.5 -- v0.1.13 (2025-11-24, tags only)

Rapid iteration on TUI UX and binary packaging.

### TUI

- **Chips bar, ranking presets, pane density, peek badge**, and persistent controls ([`8944d30`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8944d301))
- Visual feedback for modes and zero-hit suggestions ([`abdb82b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/abdb82b7))
- Global Ctrl-C handling and updated TUI keymap ([`98393aa`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/98393aaa))

### CLI

- **Binary renamed to `cass`**; default to TUI with background indexing; logs moved to file ([`196945e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/196945e8))

### Bug fixes

- UI artifacts in help overlay and F11 key conflict ([`a202ced`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a202ced8))
- Clippy lint fixes and formatting ([`b8a6ceb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b8a6ceb8))

---

## v0.1.0 -- v0.1.4 (2025-11-21 to 2025-11-24, tags only)

Initial public release and early iteration.

### Core architecture (v0.1.0)

- **Normalized data model** for multi-agent conversation unification ([`071cb0b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/071cb0b0))
- **SQLite storage layer** with schema v1 and migrations ([`03a3b06`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/03a3b063))
- **Tantivy full-text search index** with query execution and filter support ([`2cbd6a1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2cbd6a18))
- **SQLite FTS5** virtual table for dual-backend search ([`7174c33`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7174c336), [`4046a53`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4046a53e))
- **Connector framework** for agent log parsing ([`2c66016`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2c66016a))

### Connectors (v0.1.0)

- **Claude Code** connector with JSON format support ([`b755ca1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b755ca18))
- **Codex CLI** connector with JSONL rollout parsing ([`985f2ff`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/985f2ffb))
- **Cline** VS Code extension connector ([`cd5feaa`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cd5feaa8))
- **Gemini CLI** connector with checkpoint and chat log parsing ([`e49ce2d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e49ce2d6))
- **Amp** and **OpenCode** connector implementations ([`6e05e84`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6e05e84b))

### TUI (v0.1.0)

- **Three-pane TUI** with multi-mode filtering and pagination ([`8bd30b6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8bd30b68))
- Theme system, help overlay, focus states, timestamp formatting ([`410e02c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/410e02c1), [`7ec3b7a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7ec3b7a6), [`c7bce09`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c7bce092))
- Editor integration, granular filter controls, and detail views ([`6149d6d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6149d6d1))
- Contextual hotkey hints in search bar ([`fedda28`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fedda28a))

### Indexer (v0.1.0)

- Watch-mode incremental indexing with mtime high-water marks ([`cd6b2dc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cd6b2dcb))
- Persistent watch state to survive indexer restarts ([`afc1775`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/afc17756))
- Robust debounce logic for file watcher ([`7ebc48e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7ebc48e3))

### Installation (v0.1.0 -- v0.1.4)

- **Cross-platform installers**: `install.sh` (Linux/macOS) and `install.ps1` (Windows) ([`cae7d56`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cae7d56d))
- Easy mode, checksum verification, quickstart, and rustup bootstrap ([`cfac576`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cfac5764))
- Build-from-source fallback with `--from-source` flag ([`88fb89d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/88fb89d2))
- **Homebrew formula** and **Scoop manifest** ([`a49c62f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a49c62f9))
- Automated SHA256 checksum generation in release workflow ([`5cb2f92`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5cb2f92b))

### CI/CD (v0.1.0 -- v0.1.4)

- GitHub Actions workflows for CI and automated releases ([`a2bdbf1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a2bdbf1a))
- Comprehensive CI/CD pipeline with automated GitHub releases ([`f5ffbce`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f5ffbceb))
- Runtime performance benchmarks for indexing and search ([`19821ca`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/19821ca7))

### Testing (v0.1.0 -- v0.1.4)

- Comprehensive test infrastructure: connector fixtures, SqliteStorage unit tests, Ratatui snapshots, search/tracing tests, E2E index-to-TUI workflow, and installer tests ([`01cfba9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/01cfba90), [`652c5ba`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/652c5ba6), [`fa0b471`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fa0b471b), [`9c42147`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9c421472))

### Bug fixes (v0.1.0 -- v0.1.4)

- Fix snippet extraction with Tantivy `SnippetGenerator` and SQLite `snippet()` ([`a9b0241`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a9b02411))
- Critical FTS rebuild performance issue ([`d4fd6ab`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d4fd6abb))
- Gemini connector message indexing collision and deterministic file order ([`349d0bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/349d0bd6))
- Tantivy workspace field type for exact-match filtering ([`016b1dd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/016b1dd6))

---

## Pre-v0.1.0 (2025-11-20 to 2025-11-23)

Initial development. Project scaffolding, architecture design, and first implementations of the connector framework, SQLite storage, Tantivy search, and Ratatui TUI. First commit: [`2cf22a1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2cf22a19).

---

[Unreleased]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.2...HEAD
[v0.2.2]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.1...v0.2.2
[v0.2.1]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.0...v0.2.1
[v0.2.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.64...v0.2.0
[v0.1.64]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.50...v0.1.64
[v0.1.50]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.36...v0.1.50
[v0.1.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.0
