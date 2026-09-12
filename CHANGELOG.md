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

Scope window: this update covers the changes after the 2026-08-31 v0.7.1
binary release, through the 2026-09-10 v0.8.0 binary release and the unreleased
2026-09-11 follow-ups. Earlier version entries retain their existing scope.
Git commits, release metadata, and Beads supply
the evidence; [CHANGELOG_RESEARCH.md](CHANGELOG_RESEARCH.md) records coverage.

## Release Timeline

| Version | Date | Publication state |
|---------|------|-------------------|
| [v0.8.0](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.8.0) | 2026-09-10 | Published GitHub Release: Linux x86_64/arm64, macOS arm64, Windows x86_64 |
| [v0.7.1](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.7.1) | 2026-08-31 | Published GitHub Release and binary baseline for the changes below |

## [Unreleased]

### Added

- Local Shelley and Grok Bot session indexing. Their source containers are
  excluded from raw mirroring; Grok Bot containers are also excluded from
  generated remote-sync sources. Grok Bot keeps its own provider identity
  and native message IDs (#415, #447).

### Fixed

- Source-configuration backups use exclusive file creation and bounded
  collision retries, preserving an existing destination or symlink target.
- Fleet setup retains resumable progress while its final sync is pending,
  including after a failed sync or JSON-mode deferral.
- Fleet auto-discovery excludes Muse's authentication file and configuration
  directory from generated sync sources while retaining its session data.
  Existing source configurations are unchanged.
- Devin incremental scans retain messages committed within the watermark's
  whole second. WAL-only appends and replay preserve message identity (#449).
- Codebuff message revisions replace stale searchable content while preserving
  canonical message identity; replay does not duplicate messages (#423).
- Pending OMP analytics repair appears in status, validation and doctor.
  A full, unscoped analytics rebuild completes all affected projections;
  scoped repairs retain the pending state. Analytics mutations share the
  data directory's maintenance lock with indexing (#413, #426).
- Interrupted indexing can resume from durably completed source files. Reuse
  checks the parser build contract and source observations, so changed files,
  changed parsers and incomplete sources are parsed again (#426).
- Indexing handles SIGINT and SIGTERM through its progress loop, stops at
  supported durable checkpoints, and returns a retryable interrupted result.
- Legacy analytics repair commits bounded message batches and retains a resume
  cursor. Cancellation preserves completed work, and progress identifies the
  analytics phase in message units. Initial clearing of ordinary rowid tables
  is also batched; unexpected `WITHOUT ROWID` layouts retain the transactional
  fallback. These bounds do not cap total process memory (#413, #426).
- `stats` reports storage failures as errors instead of successful zero counts.
  Empty archives and unmatched source filters still return real zeros. Agent
  and workspace ordering remains deterministic; large-archive latency and
  memory requirements remain open (#452).
- Explicit Prime watch requests keep the selected file scope and classify
  `.prime/agent/sessions` correctly (#388).
- Current-schema archive opens avoid the engine's whole-database hydration
  path for already migrated archives, retaining required migration and schema
  repair checks (#443, #450).
- Semantic backfill rebuilds the known legacy hash-vector format. On Unix,
  unchanged completed backfills can skip canonical replay and publication;
  the cache checks archive, WAL, vector and producer identity, and changes
  still trigger reconciliation (#458).
- `sources reingest --source` restricts ingestion to the selected remote
  mirrors and rejects unknown names even when mixed with valid names. Reingest
  preserves local scan watermarks, so later ordinary indexing still discovers
  local history. Mirror previews no longer open the database before the index
  lock, and mirror lookup uses the same configured path keys as sync, including
  a bare `~` (bead `av59c`).
- The private fleet test harness resolves its artifact directory before
  checking repository containment. A `TMPDIR` symlink into the checkout can no
  longer place the private inventory there.

## [v0.8.0] -- 2026-09-10

### Known open at release

Two tests are red on this tree and are shipped knowingly; both are reporting
defects, not indexing or search defects.

- `swarm_proof_debt_cli_prioritizes_and_suppresses_debt`. The swarm evidence
  redactor's `absolute_path_with_spaces` rule excludes quotes and newlines from
  its character class but not spaces, so once it matches a private path it
  consumes the rest of the line. On a proof whose recorded shape is
  `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-proof cargo clippy
  --all-targets -- -D warnings` the output is
  `rch exec -- env CARGO_TARGET_DIR=/data[REDACTED_PATH]` -- the path is hidden
  and so is the command. The proof-debt classifier keys on `cargo clippy`
  without `cargo test`/`cargo check`, so it can no longer see the shape it is
  meant to flag and the `incomplete-proof-command-set` debt goes unreported.
  This is over-redaction with information loss rather than a privacy hole. The
  fix is a redaction-engine change, not a pattern tweak: a space may only
  continue a path when the following token still looks like a path, and Rust's
  `regex` has no lookahead to express that, so it has to move out of the
  pattern. Left for a change that can be reviewed on its own. The operations
  dashboard hit the same rule and is fixed below, because there the command had
  already cleared a strict allow-list.
- `timed_out_search_returns_before_slow_operation_and_names_shed_sections`. The
  scoped search-timeout retry advice is mid-flight work whose own tracker entry
  is still open; the surrounding behavior -- the retry preserving database,
  filters, time bounds and pagination -- is in this release.

Two further tests are order-dependent rather than broken --
`search_cursor_manifest_marks_rebuilding_generation_best_effort` (a concurrent
index run makes a parallel search exit `index-busy`) and
`devin_cli_indexes_searches_and_reopens_without_duplicates`. Both pass when run
alone; neither indicates a product defect.

Indexing, archive diagnostics, answer packs, maintenance commands, and dependency
updates from the reality-check bridge work. Existing archive corruption and
mixed-engine concurrent-WAL safety remain outside this release's repair claims.

### Delivered capability and evidence

**Maintenance commands and indexing.** Bookmarks and encrypted Pages key
management are reachable from the CLI. Targeted lexical reconciliation repairs
one conversation, while resume reconciliation and maintenance heartbeats make
interrupted indexing easier to recover and diagnose. Representative commits:
[bookmarks](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b4b792899803300f5202c83c03aa12b991384221),
[single-conversation reconcile](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/20970d4d00bfb45a5f45d43b19978c49a9074b6c),
[maintenance heartbeats](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/81ea0649c588972ece48c9a05584d9b4ddf7b837),
and [Pages keys, diagnostics, and installer changes](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5f05938484b7613cb531088453f43e94c4d4b9ec).

**Connector and semantic reliability.** Muse sessions gain an adapter; remote
probe/install commands use file-backed stdin; interrupted Pages publication
recovery takes the output lock. ANN descriptors no longer depend on FSVI WAL
identity, and zero-norm hash embeddings recover to usable vectors. Representative
commits: [Muse](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9a1da8e95008038a37aa5edb082ab820f238b2a1),
[remote stdin](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fb8d93a48b0cf83a98ea973b4c427878da831a38),
[Pages recovery lock](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b0e1f216b76704b88e7e3e49baead24e9a42cf0f),
[ANN identity](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/59aab8926e9d0815360793fbe9864e47ab6c4b7b),
and [hash-vector recovery](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3375db2375d1966229d25a55adcb132a5f5c92e5).

**Archive pressure and truthful readiness.** Background refresh backs off after
failed attempts, derived FTS work has time/message limits, and final WAL
checkpoints have deadlines. A matching archive fingerprint covers age-only
staleness; missing proof remains unknown, and deferred integrity checks say so.
These changes bound CASS's work and reporting; they do not certify recovery of
an already damaged archive. Representative commits:
[refresh backoff](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2651e3631441bb06c9e77ad41f9069bc14bcf39c),
[FTS budgets](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f5625f734dfbd095054c1b8157c7beb26641e23a),
[message-count limit](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f5a7c0ec64f8535b36db0652d27cd702150365eb),
[checkpoint deadline](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b06ba9d0ec16eab6b559231d5210b3d1511fab64),
[age-only staleness](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6a3985356b4a4739230d469cb19c48f31ec034ee),
and [deferred integrity status](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/63d472934856de7154680f3439b904852353029a).

**Answer-pack contracts and path isolation.** Packs verify citations against
ingested redacted text, bind IDs to verified spans, preserve literal excerpts
in Markdown, and admit output against its serialized token budget. Explicit
field/skill-content controls join those contracts. Full indexing ignores saved
incremental cutoffs, and model backfill resolves archive and asset overrides
together. Representative commits:
[serialized budget](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8ac7ee2b7018c08cc23f9999d3e37b62a0b542ad),
[citation verification](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/73ad6e5a51352d78dab52c88b6934ec65404a99d),
[pack controls](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ce2b00896470ce5982b855c56c62b32e1bbec9d6),
[full-scan cutoffs](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aba176bab4157d8b185ffe08b17f3dfb227f2b01),
and [backfill paths](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bcaaf31a0bf3412f2d2891783004e062382dbb1e).

**Engine and connector adoption.** The complete FrankenSQLite family reaches
0.3.18; FrankenSearch 0.4.3/Quill 0.2.3 uses receipt-aged segment collection;
Asupersync 0.4.10 supplies its runtime API. Franken Agent Detection 0.2.3 expands
Antigravity/Claude discovery, and CASS routes enabled adapters through watch and
quarantine retry paths. Representative commits:
[FrankenSQLite](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3babc08bc550f3137041fd13e5db87f722cbd3a4),
[FrankenSearch](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6fb902063b56be2e9cbe9e29ef7b176ce7133a7e),
[runtime and connector routing](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/257204c8d27d21fdca860bb6e79ad5d58cc29a69),
and [recovery identity quarantine](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b8e233735fd00fbcae05685d1d0752e6c2d6d280).

**Incomplete search generations and merge memory.** The latest main change
compares the live Quill document count with the completed checkpoint, reports
a severely depleted generation as `hollow`, and refuses to certify it at the
end of indexing. Merge planning charges both input bytes and the covered
document-ID range against a 1 GiB default output estimate. This bounds planned
merge outputs, not total process RSS. See the
[implementation](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6b2ab22d30892fe6f7762d851477feea0e6f80f8),
[hollow-index report #457](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/457),
and [memory report #456](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/456).

### Workstreams and acceptance

Closed workstreams include
[targeted reconcile (`qhiv2`)](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/dbe940c7d61d33d2f3ac4880e32d39d6fc4b620c/.beads/issues.jsonl#L1688),
[Pages key CLI (`ctigq`)](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/dbe940c7d61d33d2f3ac4880e32d39d6fc4b620c/.beads/issues.jsonl#L1124),
and [persistent fingerprint caching (`nsleh`)](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/dbe940c7d61d33d2f3ac4880e32d39d6fc4b620c/.beads/issues.jsonl#L1595).
The [answer-pack conformance workstream](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/dbe940c7d61d33d2f3ac4880e32d39d6fc4b620c/.beads/issues.jsonl#L369)
and [release acceptance](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/dbe940c7d61d33d2f3ac4880e32d39d6fc4b620c/.beads/issues.jsonl#L878)
remain open at this research snapshot. Landed code and targeted tests do not
mean that every acceptance row, platform, or reporter archive has been verified.

### Added

- `sources discover --tailscale` and `sources setup --tailscale` optionally add
  online tailnet peers using their IPv4 addresses. Matching SSH aliases retain
  their configuration; unavailable Tailscale produces a warning and falls back
  to SSH-config discovery. SSH authentication and host-key checks still apply.
- An opt-in real-SSH fleet test harness accepts an external private inventory
  and exercises discovery, sync, indexing, source-scoped search, replay,
  incremental append, busy-index recovery, and an unavailable source. Raw
  receipts stay outside git; unreachable hosts keep the overall result failed.
- Muse AI sessions can be discovered and indexed through the connector registry.
- Local Devin `sessions.db` ingestion is enabled through FAD's SQLite parser,
  preserving the selected message chain and the `devin` identity. Cloud session
  acquisition remains outside this integration. See [#449](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/449).
- Prime Agent's default session directory is included in Linux/macOS source
  presets and remote probe classification, with its own `prime_agent` identity
  and documented environment overrides. Targeted and configured-root watch
  support still requires the upstream root-admission repair tracked under
  [#388](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/388).
- A targeted, idempotent lexical reconciliation path replaces one conversation's
  search documents from its canonical archive rows without rebuilding the corpus.
- `cass pack --field-mask` selects the existing field projection, including the
  documented `standard` and `full` presets. `--fields` accepts the same presets.
- `cass pack --include-skill-content` explicitly includes skill payload excerpts
  while keeping credential redaction active. Default packs still exclude these
  payloads; privacy metadata reports only skill evidence retained in the output.
- `cass bookmarks add|list|search|remove|export|import` — the bookmarks module the
  README advertised is now reachable from the CLI (exit codes 13 not-found, 14 io).
- `cass search --robot` reports `_meta.lexical_degrade_reason` (`query_fuel_exhausted`)
  when a hybrid query drops its lexical leg instead of failing (GH #441).
- `cass status --json` / `cass health --json` expose `database.db_bytes`,
  `database.wal_bytes`, and `database.shm_present`, read from file metadata only,
  so an untruncated WAL is visible without a shell.
- The exit-70 stall abort now writes the standard error envelope
  (`kind: "index-stalled"`, `code: 70`, `phase`, `stall_elapsed_ms`, hint) on stderr
  before exiting (GH #439).
- `cass analytics rebuild --json` reports `wal_checkpoint` and closes its handle
  through the same checkpointing path as `cass index`.
- Robot/JSON mode prints every argument auto-correction as
  `note: auto-corrected: …` on stderr (stdout stays data-only).
- `scripts/gate.sh`: the blocking quality gate batched into one rch admission with
  per-stage receipts, pinned UBS, and positive test-count requirements. Release
  preparation runs these checks remotely; this release does not use GitHub Actions.
- `cass pages key list|add-password|add-recovery|revoke|rotate --archive <bundle>`:
  key-slot management for exported encrypted bundles over the previously uncalled
  key-management engine; passwords via prompt or `--password-stdin`, never argv;
  `docs/RECOVERY.md` rewritten to the real surface.
- `cass doctor` checks `archive_wal` (an untruncated WAL sidecar above 64 MiB is
  reported with its size; `--fix` checkpoints it and reports pass, blocked, or
  failed truthfully) and `index_segments` (a lexical generation fragmented past
  8× the merge threshold is reported with the incremental `cass index` remedy,
  whose maintenance pass folds it in place).
- `cass index --gc` (GH #453): run the lexical engine's grace-period garbage
  sweep on its own and report the merge-retired segment files and bytes it
  reclaimed (`--json` for automation). Every incremental `cass index` performs
  the same sweep at open; a folded segment file is unlinked only once no
  published MANIFEST generation has referenced it for the engine's 300 s grace
  period, so readers on the previous generation are never broken.
- `cass doctor --json` `storage_pressure.full_rebuild_readiness` reports
  `retired_segment_bytes`, `retired_segment_files`, and
  `retained_publish_backup_bytes` next to the live `lexical_index_bytes`, with a
  note naming the path that reclaims each class (GH #453).
- `cass status --json` / `cass health --json` report `index.segment_files`, a
  metadata-only upper bound on the published lexical segment count.
- `install.sh` probes the host's glibc before downloading a Linux prebuilt
  binary and falls back to build-from-source below 2.38 (proven on Ubuntu 22.04
  and 24.04 containers).
- `cass doctor --fix`'s WAL checkpoint runs under a wall-clock deadline (120 s,
  `CASS_DOCTOR_WAL_CHECKPOINT_TIMEOUT_SECS`); on an archive whose frankensqlite
  writable open loops (GH #382) the `archive_wal` check fails truthfully, names
  the issue and the stock-sqlite remedy, and doctor returns instead of hanging.
  The read-only warning names the same loop shape.
- End-to-end proofs for GH #439 (a slow-but-heartbeating post-publish FTS repair
  survives the stall window; a parked one exits 70 with the envelope), GH #440
  (a force rebuild killed between the staged commit and the checkpoint write
  resumes with a plain `cass index`, no duplicate identities), and GH #441 (a
  40-segment generation is folded by a plain `cass index`).

### Fixed

- The operations dashboard's "next proof" command is runnable again. Every
  dynamic field on that surface went through the swarm path redactor, so the
  one field whose entire purpose is to be copied and executed came out as
  `CARGO_TARGET_DIR=[REDACTED_PATH]` on any host with a matching path root --
  and looked correct on a developer machine that had none. The command is
  rendered verbatim now. It is not a redaction hole: it is only rendered at all
  after clearing an allow-list that rejects every shell metacharacter and then
  admits exactly two shapes, a pinned read-only `cass` subcommand carrying
  `--json`/`--robot` with no mutating flag, or `rch exec -- env
  CARGO_TARGET_DIR=/data/tmp/cass-<suffix> cargo test|check|clippy|bench|fmt`
  -- a path prefix the validator itself hard-codes. Neighbouring free-text
  fields keep the redactor, and HTML escaping is unchanged.
- Fleet discovery keeps `Match` options out of preceding SSH host metadata,
  handles quoted aliases and comments, and retains first-value precedence.
  `--skip-existing` compares connection targets instead of source labels; the
  displayed add-source command uses valid positional and option syntax.
- Setup probes share a bounded budget for optional health, statistics and
  directory measurements, so large archives do not consume the whole SSH
  deadline. Missing measurements remain unknown, and an incomplete index
  inspection does not trigger remote reindexing. On hosts without `timeout`
  or `gtimeout`, setup retains detected paths and skips these measurements.
- Tailscale status output is bounded during pipe collection, with child cleanup
  on collection errors. Null or missing peer address lists are treated as empty,
  so a peer without addresses does not disable discovery. The live fleet harness
  refuses optimized Python runs
  that would disable its checks, protects private initialization errors, and
  compares global and source-filtered content and provenance.

- A pre-scan authoritative lexical repair no longer strands its own run's
  checkpoint. The repair rebuilds the lexical index from the canonical database
  and persists an *exact* completed checkpoint, and the run then continues into
  the incremental source scan --- an ordering unique to this path, since the
  canonical-only full rebuild runs no scan and the post-scan rebuilds run after
  ingest. When that follow-up scan ingested new rows the end-of-run refresh was
  skipped as already-exact, so the checkpoint kept the pre-scan
  `COUNT`/`MAX(id)` content fingerprint: search's fingerprint check reported
  "storage fingerprint no longer matches" and `cass status` reported the lexical
  assets stale until some later run happened to rewrite the checkpoint. The run
  now re-derives its exact canonical totals from the post-scan database and
  refreshes the checkpoint before exiting; the totals matter as much as the
  refresh, because the final refresh consumes them verbatim. Follow-on to
  [GH #457](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/457).
- SSH host discovery uses `CASS_SSH_CONFIG`, matching the transport commands,
  and follows included configuration files with bounded recursion and duplicate
  alias handling. A private configuration no longer yields an empty discovery
  result while direct connections through the same file work (bead `av59c`).
- `sources sync --json` and `sources reingest --json` return one document with
  the nested indexing result. Indexing failures report `status: "index_failed"`
  and retain the nonzero exit code, rather than printing a premature sync
  success and a second JSON document (bead `av59c`).
- Doctor preserves multiple `quick_check` findings instead of replacing them
  with a single-row query error. It inspects every returned row before declaring
  health, bounds displayed diagnostics, and rejects empty or malformed results.
  The same check runs after candidate promotion (bead `9lz4y`).
- Search timeout retry commands retain the selected database, read-only policy,
  agent/workspace/source/session filters, resolved time bounds, pagination,
  semantic options and robot output limits. Both setup and metadata timeouts
  use the same scoped retry; stdin-scoped searches still omit replay advice
  ([GH #422](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/422)).
- Incremental indexing no longer advances a connector's scan watermark after
  a conversation is deferred or quarantined without being persisted. Unrelated
  completed connectors can still advance; a later successful retry saves the
  missing messages without duplicating them. Streaming and batch paths share
  this rule (GH #426). The full per-source resume ledger remains unfinished.
- The primary database writer recognizes the FTS virtual table by catalog
  type instead of requiring a positive B-tree root page. New and appended
  messages now reach an existing fallback search shadow without a rebuild;
  replay remains deduplicated and shadow-size limits still apply
  ([`ca621f9d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ca621f9d2a3d48ffcc18447132ee4e15787ef61a), bead `igh4d`).
- Semantic backfill reuses compatible vectors across intervening ingest and
  compares current document content and provenance before retaining them.
  Missing checkpoint files restart coverage safely; partial repairs revoke
  stale readiness, and publication retains the prior WAL before validating the
  replacement. Backfills share the indexing lock and report real progress.
  Seven storage/vector regressions and a native MiniLM lifecycle pass. The
  native probe covers six documents across seven bounded quality batches,
  intervening ingest, and exact lexical/semantic source identity agreement.
  The reporter subsequently confirmed progress surviving ingest and publication
  of 425,762 quality vectors on the original roughly 547,000-message archive.
  Their maintenance batches still spend about five minutes walking canonical
  identities; this is field-reported correctness evidence, not a throughput win.
  See [the field results](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/458#issuecomment-5604445676).
  See [#458](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/458).
- Watches of an explicit Devin database follow its WAL/SHM events without
  ingesting neighboring databases. Provider timestamps older than filesystem
  events no longer hide later WAL commits. See [#449](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/449).
- Watch mode retains files deferred as actively written and retries them after
  the safety window even if no further filesystem event arrives. Reported
  reindex failures keep those files queued for retry. See [#455](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/455).
- Legacy incomplete lexical checkpoints apply the fallback FTS shadow's corpus
  bound before opening readonly rebuild workers, matching the ordinary indexing
  path. This addresses the restart-path bypass in [#413](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/413),
  without claiming that every archive-scale memory or doctor-open problem is resolved.
- Completed lexical resumes publish exact canonical message counts and replace
  pending fingerprints before returning, including canonical messages omitted
  from lexical search. An ordinary search is no longer needed to repair that
  metadata. See the follow-up in [#440](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/440).
- Doctor distinguishes a queryable FTS table from verified structural integrity;
  a successful query no longer dismisses a structural checker failure as a
  harmless engine difference. See [#438](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/438).
- Introspection describes the emitted rebuild engine-compatibility and orphan
  raw-blob counters. Triage schemas distinguish uninspected sections and unknown
  values when its budget expires, while ordinary status schemas retain their
  observed-value types. The real CLI contract regression includes a minimal
  accepted triage budget; the corrected schema test and all 68 contract goldens
  pass on the remote candidate.
- Swarm trace redaction covers temporary archive paths on Linux, macOS and
  Windows, including configured temporary directories under the user's home.
  It applies full-path redaction before home-prefix shortening. This corrects
  a path leak found by the full suite; the unchanged CLI privacy regression
  and new path cases pass remotely, along with all 64 trace CLI tests. The
  mandatory scanner gate remains unresolved, so this is not release clearance.
- Cursor workspace repair preserves session/message identity and stored token
  and cost amounts while correcting canonical attribution and analytics totals.
  Replay repairs stale analytics without inserting duplicate messages; inconsistent
  rollups cause the transaction to roll back. Storage and real CLI regressions
  pass with an upstream connector candidate whose trusted-workspace parsing is
  not yet published. See [#459](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/459).
- A hollow published Quill generation — one serving a handful of documents
  while its completed rebuild checkpoint, content fingerprint and generation
  manifest all still look right — no longer reads as healthy (GH #457).
  Readiness now compares the live document count from the engine's MANIFEST
  (metadata only, no segment open) against the count the completed checkpoint
  certified: `cass status`/`cass health` report `index.status: "hollow"`,
  `index.hollow`, `index.live_documents`, an `index hollow` error and a
  `cass index` recommendation; `cass doctor check` warns in
  `index_sync` (also when the index serves under 10% of the archive's
  messages); readiness projections classify it as rebuild-now. On the publish
  side, a rebuild's post-publish document-count proof fails when the count
  cannot be observed instead of being skipped, the direct-commit rebuild writes
  the generation manifest before marking its checkpoint completed, and every
  `cass index` run re-checks the served count at its end and refuses to
  certify a hollow generation (a plain `cass index` already rebuilds one it
  finds at startup through the pre-scan sparse-index repair).
- Lexical merge runs are planned under a byte cap (GH #456):
  `CASS_LEXICAL_MERGE_MAX_OUTPUT_BYTES`, default 1 GiB, charged with the docid
  hull a Q1-preserving concat merge carries. The end-of-rebuild fold used to
  merge every segment into one and the incremental fold could plan one run over
  a fully fragmented generation; the engine assembles each merge output as one
  in-memory buffer sized to the finished file, so the fold's memory scaled with
  the whole index (GH #453's 6.75 GB single segment). A generation now
  is folded in bounded planned runs; oversized singleton segments are left
  unmerged. The cap controls estimated merge-output allocation, not the total
  RSS of indexing or every engine allocation.
- Route every enabled connector through filesystem watching and quarantine
  retries, including Prime Agent, Kiro, Devin, OpenHands, Goose, Crush, and Hermes.
  The candidate also enables FAD's optional `devin` parser. Dispatch registration
  alone does not establish every provider's watch-root behavior; the remaining
  Prime and Devin watch cases stay tracked in their original issues.
- HTML export and robot diagnostics use specific Prime Agent, Kiro, and Devin
  identities instead of generic provider labels.
- Make the release formatting gate fail when rustfmt aborts or is killed.
- Register the FTS shadow viability startup phase so watch indexing reaches the
  scan instead of panicking during preflight.
- Redact the archive directory inside doctor baseline explanations as well as
  path fields.
- Preserve requested search mode and report skipped optional sections when the
  robot deadline expires during setup. Session-filter resolution now uses a
  bounded, strictly read-only archive query in robot mode.
- Keep `cass --version` parseable as a plain semantic version.
- Extract repository lessons lazily when pack evidence actually references a
  known commit or closed bead, avoiding that work for unrelated evidence.
- Defer pack Git-history traversal until an explicit identifier needs it. The
  deferred lookup uses the HEAD captured when the query's correlation index was
  constructed, so a concurrent commit cannot change its view.
- `cass models backfill` honors `--data-dir` and `--db` when choosing the archive
  and publishing semantic assets, including separate archive/asset directories.
- An old index with a matching archive fingerprint remains ready; age alone no
  longer forces a refresh. Missing timestamps or fingerprint mismatches still
  report the appropriate incomplete/stale state (GH #452).
- Search keeps serving a readable lexical generation when duplicate fallback
  FTS schema rows prevent archive fingerprinting. Schema repair remains on
  the indexing path.
- `cass doctor --recover-from-archive` quarantines a canonical row whose
  identity columns (`agent_slug`, `workspace`, `external_id`, `source_path`,
  `source_id`, `origin_host`) held a non-text value, or whose `source_path`
  was NULL or empty, instead of exporting it under a synthetic identity
  (GH #391). Those values violate CASS's identity contract; their types alone
  do not prove page aliasing or establish which messages belong to the row.
  The row is counted
  (`rows_quarantined` in the JSON envelope and the text summary), listed in
  `sessions` with a `quarantined canonical row` reason and its coercions,
  and paging continues past it; a row whose only coercions are title or
  timestamp columns is still exported and reported under `rows_coerced`.
- Full indexing and deliberate repair scans now ignore saved connector cutoffs,
  so older local sessions are rescanned as requested. Incremental scans retain
  their connector-specific timestamps.
- Search hydrates message IDs using validated integer literals, avoiding the
  FrankenSQLite parameterized-`IN` query shape that could return no message
  bodies even when Quill found matching documents
  ([hydration fix](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6057b8e4ab8768052ae5321ede03413b4cac2e7e)).
- Answer packs verify citations against ingested, redacted source messages,
  bind evidence IDs to the verified spans, and retain the same excerpts across
  JSON and Markdown projections. Whole-message skill and hook injections are
  excluded before truncation. The serialized output must fit the requested
  token budget; an envelope that cannot retain required evidence returns
  `pack-budget-too-small` (exit 2).
- Answer-pack source verification receives its phase allowance after planning
  completes, capped by the remaining request budget. Slow planning no longer
  consumes the citation-check allowance before verification starts.
- The full-rebuild headroom preflight (`cass index --full`, doctor's
  `full_rebuild_readiness`) doubles only the LIVE lexical bytes — what the
  current MANIFEST references — instead of the recursive size of `index/`
  (GH #453). Merge-retired segment files awaiting the engine sweep and prior
  generations under `index/.lexical-publish-backups/` are already on disk and
  are not rewritten by a rebuild; counting them twice refused a rebuild the
  disk could hold (48 GB demanded, 37 GB needed, 43 GB free on the reporting
  archive). The `index_segments` warning and the query-fuel hint name the
  incremental `cass index` remedy instead of the rebuild the shortfall blocks.
- `cass status --json` / `cass health --json` no longer report a lexical
  checkpoint as healthy while search defers repair on a storage-fingerprint
  mismatch (GH #353). The skip-open surfaces (health watermark lane,
  count-less status) now compare the same fingerprint search defers on,
  served from the identity-keyed sidecar cache without ever opening the
  archive — an absent sidecar stays an honest null instead of an
  assumed-good — and the search-side deferral warning names the repair that
  actually runs: `cass index --full` on an archive above the
  incremental-repair size policy, where plain `cass index` defers the same
  repair.
- Stale-on-read auto-refresh no longer respawns a doomed background catch-up
  on every read. On 2026-09-03 a catch-up spawned by an agent's `cass search`
  inherited that session's 16 GB cgroup, crawled the FTS5 shadow write path
  for an hour on a 10 GB archive, was OOM-killed at 14 GB without advancing
  `last_indexed_at`, and the next stale read respawned it. The spawner now
  judges the previous spawn against the index watermark: a spawn the index did
  not outlive counts as a failure (1 h, then 6 h before the next attempt;
  three in a row trip the breaker until any run completes). Surfaced as
  `auto_refresh.outcome` = `backed_off`/`tripped` with `consecutive_failures`
  and `detail` in `--robot-meta`/`status --json`, in `schedule status`, and as
  a `doctor` warning.
- An index run whose inline `fts_messages` shadow writes exceed a budget
  (`CASS_FTS_INLINE_BUDGET_SECS`, default 300 s) skips the shadow for the rest
  of the run instead of crawling for hours: canonical rows and the Quill index
  still land and `last_indexed_at` advances; the shadow (read only by the SQL
  search fallback) is left Partial with the reason persisted for `doctor`. The
  paged shadow repair/rebuild gets a per-page budget
  (`CASS_FTS_REPAIR_PAGE_BUDGET_SECS`, default 120 s) so `index --full` and
  `doctor --fix` stop truthfully at the same engine wall (GH #413,
  frankensqlite#405/#406).
- The `fts_messages` SQL-fallback shadow is bounded by corpus size
  (`CASS_FTS_SHADOW_MAX_MESSAGES`, default 100,000 indexable messages).
  Measured on a copy of a 10 GB, 631k-message archive: fsqlite re-tokenizes
  the whole shadow into memory at every memdb reload after a write
  transaction (frankensqlite#408), 20 GB resident and about two minutes per
  statement boundary before cass has indexed anything, and every later write
  transaction clones it — the actual reason the owner's background catch-ups
  died and spent an hour in `preparing`. Above the bound the index run
  drops the shadow before its first write (deferred-FTS5 connection, nothing
  hydrated), a run whose ingest crosses the bound stops feeding it and drops
  it at finalize, `index --full`/`doctor --fix` refuse to recreate it while
  the corpus is over the bound (nonfatal, reason persisted), and `doctor`'s
  `fts_table` check says the drop was deliberate. Quill is the lexical engine;
  the SQL fallback scans `messages` when no shadow exists.
- `cass doctor --json` reports `reason_code: "integrity_unchecked"` (also in
  `degraded_reason_codes`) when the deep page-integrity probe was deferred, so
  an agent no longer reads a `healthy` status on a large archive as "the
  archive is verified"; `status`/`healthy` keep their fail-count contract.
- Avoid a per-statement FTS savepoint clone in batch inserts (GH #413).
  On the earlier engine, every `INSERT INTO fts_messages` statement was
  making frankensqlite deep-clone the whole in-memory FTS5 table for its
  per-statement savepoint (O(table) per statement, a ~35 GB transient at
  538k rows). The batch insert now skips the statement savepoint; the batch
  transaction remains the rollback boundary. The adopted engine also includes
  the upstream FTS savepoint undo-log fix. This is not a guarantee that every
  reported large-archive stall is resolved.
- Archive recovery pages by `id`, records unreadable IDs and failed message
  reconstruction, and continues when a total-count query fails. Identity-column
  type violations are quarantined as described above; recovery no longer
  coerces those fields into apparently valid session identities (GH #391).
- Antigravity source presets recognize the IDE store (GH #454). The default
  source presets, `cass resume` agent detection, and the docs now cover the
  IDE store `~/.gemini/antigravity/` alongside the `agy` CLI store
  `~/.gemini/antigravity-cli/`. The published `franken-agent-detection` 0.2.3
  probes both roots and keys IDE conversations as `ide/<uuid>`.
  A conversation re-keyed from the bare UUID
  will reuse its existing canonical row by transcript path instead of
  duplicating it.

### Changed
- **Linux release binaries are cross-built with `cargo zigbuild` at a pinned
  `GLIBC_2.28` ABI floor** instead of natively on the release host. A native
  build inherits the host's glibc, and every build host now runs 2.42/2.43:
  `acosf`, `asinf`, `coshf`, `log10f` and `sinhf` picked up glibc 2.43's new
  single-precision libm symbol version, so the binary demanded `GLIBC_2.43`.
  `install.sh` admitted any host at the old `MIN_GLIBC=2.38`, which meant
  Ubuntu 24.04 LTS (2.39), 25.04 (2.41) and 25.10 (2.42) would have installed
  it and then failed at load. `MIN_GLIBC` now reads `2.28` to match the
  measured artifacts, which also restores prebuilt support for Debian 12
  (2.36), Ubuntu 22.04 LTS (2.35), RHEL/Rocky 9 (2.34) and Amazon Linux 2023
  (2.34) -- all of which the old floor pushed to a source build. Verified with
  `objdump -p` and `readelf -V` on both shipped Linux binaries. macOS and
  Windows artifacts are unaffected.
- The lockfile adopts `chacha20` 0.10.2 and `libssh2-sys` 0.3.3; crypto
  vectors, round-trip properties, and the real Docker SFTP fallback passed
  with these patch updates.
- `asupersync` is pinned to crates.io `=0.4.10`, which publishes the
  `Cx::is_cancelled` API required by FrankenSearch 0.4.3.
- `frankensearch` is pinned to `=0.4.3` (Quill `0.2.3`). Segment collection
  clocks the 300 s grace period from each retirement receipt, allowing
  `cass index --gc` to reclaim old folded inputs while newer generations
  continue publishing. This release deliberately **holds** that pin rather
  than taking the newer published `0.5.0` / Quill `0.2.4`: the GC fix
  `cass index --gc` depends on shipped in `0.4.3`, so `0.5.0` adds nothing
  for it, while the two upstream fixes that would justify moving --- the
  corrected `asupersync >= 0.4.10` floor declaration and the live-document
  publication floor that discards a failed replacement ingest instead of
  committing a tombstone-only MANIFEST (the engine half of GH #457) --- are
  on frankensearch `main` and are *not* in `0.5.0`. Taking `0.5.0` today
  would also pull the `frankensearch-rerank` `0.3.0` semver-major and stamp a
  new engine version into every fresh MANIFEST for no correctness gain. cass
  carries its own consumer-side hollow-generation detection either way.
- `franken-agent-detection` pinned from crates.io `=0.2.2` to `=0.2.3`: the
  Antigravity connector probes the IDE store (`~/.gemini/antigravity`) as
  well as the `agy` CLI store, so IDE sessions index without
  `CASS_ANTIGRAVITY_DATA_ROOT` (GH #454); Claude Code detection honors
  `CLAUDE_CONFIG_DIR` / `XDG_CONFIG_HOME` (GH #448); Codex token usage is
  read from real rollouts; Claude tool results are kept as `role:"tool"`
  messages; Cursor/OpenCode mirrors are deduped; the 100 MB scan cap applies
  to every connector; Shelley discovery names the canonical database path
  like scan. The optional `devin` feature is enabled for local store ingestion.
- Upgraded the complete FrankenSQLite family from the `0.3.13` line in v0.7.1
  to registry `0.3.18`. This adds parameterized rowid seeks (GH#415/cass#382),
  read-only WAL byte/timestamp preservation, reader-registration error
  propagation and I/O buffer lifetime fixes. It retains incremental WAL-tail
  folding (GH#382), reserved lock-byte/freelist repair (GH#410), FTS metadata and foreign-write
  visibility fixes (GH#408), incremental FTS segment writes and savepoint undo
  logs, prefix-BM25 ranking fixes, and prepared-read schema-retry cleanup.
  All 20 family crates share the exact pin.
  These fixes do not establish repair of an already corrupted archive, and
  upstream GH#411 mixed-engine concurrent-WAL safety remains unresolved.
- The index run's final `wal_checkpoint(TRUNCATE)` runs under a wall-clock
  budget (900 s, `CASS_INDEX_FINAL_WAL_CHECKPOINT_TIMEOUT_SECS`): on an archive
  whose frankensqlite writable path loops (GH #382) the run no longer hangs
  after a successful publish; the WAL is left for the next opener and a
  warning names the remedy. The loop itself is fixed upstream in frankensqlite
  [the WAL-tail indexing fix](https://github.com/Dicklesworthstone/frankensqlite/commit/8d012706a),
  included in the `0.3.18` engine consumed here.
- The TUI analytics dashboard's load task is one production function with the
  detached-rebuild spawn injected (`load_chart_data_with_auto_rebuild`); the
  test-only stub that returned canned data is gone, and unit tests prove that
  missing rollups spawn exactly one detached `cass analytics rebuild` and never
  rebuild in-process (GH #395).
- Quill is opened everywhere with one cass configuration: engine visibility-lag
  publication is disabled (snapshots publish only on cass commits), the query fuel
  budget stays at the engine default with `CASS_QUILL_QUERY_FUEL_BUDGET` as an
  escape hatch, incremental runs fold published runs when they exceed the merge
  threshold, and `cass index --full` consolidates the generation (GH #440, #441).
- `cass health` uses the same strict, mutation-free, 30 s-bounded owner-thread
  archive probe as `cass status`; it never checkpoints a dirty WAL.
- The default `cass search` no longer re-reads the archive WAL on every call: the
  lexical self-heal fingerprint is memoized on the archive's physical identity in
  `<index>/.archive-fingerprint-cache.json`.
- The TUI loads the semantic context lazily before the first frame and spawns
  analytics rollup rebuilds as a detached `cass analytics rebuild` child (GH #395).
- The post-publish FTS shadow rebuild ticks the stall watchdog per page (GH #439).
- `#![deny(unsafe_code)]` outside tests with scoped, commented allows on the
  audited FFI/env sites.
- The Rust toolchain is pinned to `nightly-2026-08-31`.
- README and AGENTS.md corrected against the code (schema v20, key bindings, dedup
  keys, `--timeout` semantics, swarm fixture-only status, export flags, installer and
  self-update reality, hidden subcommands documented).

### Validation and documentation

- `main` compiled again after a stale renamed constant left by concurrent edits
  (`CASS_QUILL_QUERY_FUEL_BUDGET_ENV`).
- Golden tests no longer absorb a host's real Pi Agent sessions. The writer was
  the robot-CLI test binary, whose tests use the committed fixture archive in
  place: stale-on-read auto-refresh indexed the host's sessions into it. Those
  tests now run with `CASS_AUTO_REFRESH=0`; the golden harness also pins
  `PI_SESSIONS_DIR`, `PI_CODING_AGENT_DIR`, and `PI_CODING_AGENT_SESSION_DIR`.
- `forget --apply`, `dedup --apply`, and `analytics validate --fix` close their
  archive handle through the same checkpointing path as `cass index`, so the
  next opener does not replay their rewrites from the WAL.
- `cass introspect`/`capabilities` report `has_json_output: true` for commands
  whose JSON flag lives on a subcommand (bookmarks, pages key, sources, …).
- The `--highlight` flag doc and README no longer claim `<mark>` wrapping for an
  HTML output that search does not have; only `**bold**` markers exist.
- The unreachable `Alt+W => swarm cockpit` key arm (shadowed by the documented
  workspace-filter palette) was removed.
- Connector/index fixtures use explicit roots and child-process environments,
  reducing process-global environment leakage. The explicit Docker SFTP test
  requires the native transport and exact transferred file bytes; unavailable
  Docker or failed setup cannot count as a successful transfer.
- Rust tests use the repository's 128 MiB stack reservation. The gate rejects
  rustfmt termination, missing terminal receipts, and zero-test selections.
- A real-process regression covers a search-triggered lexical refresh that
  stalls after committing a batch while its heartbeat continues (GH #422).
  It verifies exit 70, lock release, and a subsequent cold query rebuilding
  complete search assets without losing or duplicating canonical messages.
  This validates the existing watchdog on a controlled fixture; acceptance
  on the reporter's large archive remains open.
- Release validation is still in progress. Existing targeted passes do not
  constitute a full-suite or four-platform release verdict; strict UBS remains
  blocking, and full-history pack latency has not met its unchanged SLO on the
  retained loaded-host run. No performance improvement is certified here.

## [v0.7.1] -- 2026-08-31

> crates.io `0.7.0` was published from commit `1f6cdcf9` on 2026-08-25;
> `v0.7.1` is the first tagged GitHub Release of the 0.7 line (binaries for
> that crates.io publish were never shipped, so `v0.7.1` supersedes it).

### Added

- Added OS-native scheduled maintenance and stale-on-read catch-up, with
  launchd/systemd installation, bounded nightly model backfill, machine-pressure
  gates, daemon-driven refresh, truthful robot metadata, and a pure-read
  `search --no-maintenance` escape hatch
  ([`81516c01`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/81516c01),
  [`f58d9f61`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f58d9f61),
  [`fdac5715`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fdac5715)).
- Replaced full-copy raw-mirror snapshots for sources at least 8 MiB with
  byte-exact 4 MiB content-addressed chunks. Growing JSONL and mutable SQLite
  sources now reuse unchanged regions across versions; verification,
  reconstruction, pruning, doctor diagnostics, and legacy whole-blob manifests
  all understand the mixed layout
  ([`402515f8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/402515f8),
  [`56a5ad57`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/56a5ad57),
  [#430](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/430)).
- Added per-archive protocol-v2 daemon attestation for embedding and reranking,
  including pinned connection identity, auto-spawned verified rerank clients,
  and explicit local fallback when attestation cannot be established
  ([`fbabc4e2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fbabc4e2),
  [`2139e73e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2139e73e)).
- Added a binary-only `selftest` error/JSON contract so packaging gates can
  distinguish executable failure from archive readiness
  ([`5adce792`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5adce792)).

### Changed

- Advanced the FrankenSQLite engine pin to `0.3.13` (rev `2d8a68b9`), which
  carries the autoindex-vanish corruption-writer fixes — the writer-side class
  behind the FTS5-corrupt-archive refusals tracked in
  [#434](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/434)
  ([`39d85c9f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/39d85c9f)).
- Advanced the registry dependency line to FrankenSQLite `0.3.11` and
  Frankensearch `0.4.1`. This carries the Windows read-only reopen repair and
  native Quill `LockFileEx` writer admission, and the build contract now rejects
  a stale or mixed lockfile family
  ([`e16d39a2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e16d39a2),
  [#402](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/402),
  [#429](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/429)).
- Reworked analytics and daily-stat rebuilds into short keyset-paginated
  transactions with in-memory dimension maps and canonical message-byte
  semantics. Filtered-out pages advance the cursor, orphan messages retain the
  former inner-join behavior, and the long rebuild-spanning write transaction is
  gone
  ([`58072668`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/58072668),
  [`722f3f35`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/722f3f35),
  [#424](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/424)).
- Reduced indexing queue and canonical-replay memory retention, tightened
  working-set reservations after responsiveness-governor clamps, and cached the
  expensive macOS physical-footprint probe
  ([`4937c134`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4937c134),
  [`1bfc929b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1bfc929b),
  [`ac49507d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ac49507d)).

### Fixed
- Fixed the GH#413 index-wedge class: the lexical-rebuild sink now flushes on
  starvation so retained byte reservations cannot deadlock the pipeline
  ([`424765b3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/424765b3)),
  and the stall watchdog grants a bounded preparing-phase grace window while
  process block IO still advances
  ([`f2f0aaff`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f2f0aaff),
  [`cd419c6f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cd419c6f),
  [#413](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/413)).
- A successful `cass index` run now validates that searchable lexical
  artifacts were actually published (exit 9 otherwise), closing the
  silent-success half of the search-refresh liveness report; runs against a
  provably empty canonical archive are exempt
  ([`f6d1e0ab`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f6d1e0ab),
  [`9157effc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9157effc),
  [`5dce0f61`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5dce0f61),
  [#422](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/422)).
- Metadata refresh is now gated on a noise-adjusted document count and the
  archive DB is probed before taking the index lock, so a refresh can no
  longer thrash on noise deltas or hold the lock against an unopenable DB
  ([`02a3165e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/02a3165e),
  [#435](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/435),
  [#436](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/436)).
- Fixed the unsafe watermark portions of the interrupted-incremental-index
  report: streaming mode advances each connector watermark only after that
  connector completes and its batches commit, batch mode no longer advances a
  timed global watermark while other connectors' batches are pending, and an
  active/skipped source withholds only its own connector watermark. The exact
  per-file resume ledger remains open pending an upstream connector seam
  ([#426](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/426)).
- Fixed empty-query browse-by-date ordering at TUI startup and made
  browse paging skip conversations with no messages, so an empty newest
  conversation cannot steal a page slot
  ([`ca088e9d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ca088e9d),
  [`c728035e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c728035e),
  [#395](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/395)).
- Doctor now scales the archive-DB probe budget with database and WAL bundle
  size and bounds the raw-mirror manifest listing by default, so large
  archives cannot time out or flood the report
  ([`91ddd68d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/91ddd68d),
  [`5f288040`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5f288040),
  [`b6c245e8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b6c245e8),
  [#374](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/374)).
- `cass status` reports a legacy Tantivy generation as engine-incompatible
  with the exact rebuild contract instead of ready, and the status lane uses a
  strict read-only, hard-bounded archive probe
  ([`b61af339`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b61af339),
  [`f6bad77f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f6bad77f),
  [#382](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/382)).
- Completed kind-first local/remote provenance across analytics, incident
  discovery, stats, timeline, semantic filtering, TUI local-file actions, and
  doctor/raw-mirror diagnostics, and storage ingestion. Named local-kind
  sources such as backup roots now remain local without collapsing their
  stable source IDs; incomplete legacy conversation metadata no longer
  overwrites authoritative source-registry kinds, while exact-source filters
  and source menus still preserve those identities (bead `rril3`, follow-up to
  `5bf29`).
- Bounded lexical-rebuild preparation by the cumulative per-conversation
  content ceiling, made the guardrail fallback prepare one conversation at a
  time, and compiled CASS's projection to FrankenSQLite's
  `ColumnSubstrPrefix`/`ColumnOctetLength` fast paths. Interrupted legacy
  checkpoints with no recorded execution mode now take the count-free
  restart-from-zero route instead of hydrating a multi-gigabyte archive during
  phase zero
  ([`f8d1cdb7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f8d1cdb7),
  [`f273ccc4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f273ccc4),
  [`bc39bf94`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bc39bf94),
  [#413](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/413)).
- Made Windows startup and shutdown viable under MSVC: the binary and index
  worker receive explicit stack budgets, cached FrankenSQLite/Quill bridge
  runtimes shut down before CRT thread-local destruction, and platform-specific
  path/lock code compiles without Unix-only assumptions
  ([`d9477c69`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d9477c69)).
- Hardened transient search-map recovery and semantic marker handling: a
  temporary map lock now requires a complete reopen before success, reload
  completion observations are rate-limited and accurate, and unreadable
  semantic markers fail closed rather than admitting stale vectors
  ([`3fcfb995`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3fcfb995),
  [`d959f035`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d959f035),
  [`20d92acf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/20d92acf)).
- Secured daemon spawn-lock creation with no-follow semantics and private mode,
  and made schedule reinstall remove obsolete jobs such as a previously enabled
  nightly timer
  ([`876d816e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/876d816e),
  [`72b15ebd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/72b15ebd)).
- Hardened first-class OMP v18 ownership and resume behavior for relocated XDG
  stores, named profiles, copied archives, and Pi-family ambiguity. Legacy
  identity reclassification now uses the same provider-qualified path evidence.
- Made nested analytics-deferral guards concurrency-safe and preserved explicit
  analytics update dispositions during indexing.
- Tightened lesson and repro-capsule secret redaction, including valid `=`
  characters in email local parts without swallowing known metadata prefixes.
- Hardened bounded quarantine retry and recovery, Pages bundle verification and
  publication, semantic fail-open readiness, installer argument and checksum
  behavior, and update-check state locking.
- Pinned the release toolchain to `nightly-2026-08-20` and tightened
  exact-version release and package verification.

## [v0.7.0] -- 2026-08-25

> Note: this section accumulated since **v0.6.25**. `v0.6.26` was tagged
> without moving its notes out of `[Unreleased]`, so the entries below cover
> both the v0.6.26 line and the work released here.

### Added

- **First-class Oh My Pi (`omp`) v18 support** ([#411](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/411)).
  cass now indexes OMP as its own agent instead of folding `.omp` transcripts
  into `pi_agent`. Discovery covers the default store, every named profile,
  existence-gated XDG stores, direct session/agent-directory overrides, and
  per-session sub-agent transcripts. OMP is wired through incremental watch,
  remote-source presets and probing, capabilities/diagnostics, token
  extraction, profile-aware `cass resume`, the TUI, and HTML export. On first
  index after upgrade, canonical legacy `.omp/agent` rows are reclassified in
  SQLite and the derived lexical/analytics assets are rebuilt, preventing one
  transcript from surviving under both `pi_agent` and `omp`.
- **Build commit embedded in the binary** ([#399](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/399)).
  `cass --version` now prints `cass <semver> (<short-sha> <commit-date>)` for
  builds made from a git checkout (with a `-dirty` suffix for uncommitted
  worktrees), so a `main` build and the nearest tagged release are no longer
  indistinguishable at runtime. `capabilities --json` and `api-version --json`
  gain machine-readable `build_commit` / `build_commit_date` fields
  (`"unknown"` for builds without git metadata, e.g. crates.io). The commit is
  resolved at
  compile time against the crate's own checkout — never at runtime against the
  process CWD.
- **`cass health --binary-only`** ([#398](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/398)).
  Same readiness report, but the exit code reflects only whether the
  executable itself works — archive conditions (stale index, active rebuild,
  uninitialized or degraded archive) are still fully reported yet never fail
  the exit code. Built for binary promotion gates and packaging smoke tests
  that must validate a candidate binary on a host whose archive happens to be
  stale. The JSON payload carries the active `exit_policy`
  (`"archive"`/`"binary-only"`).

### Changed

- **FrankenSQLite family pin `=0.3.4` -> `=0.3.8`**
  ([`5a09be4e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5a09be4e)).
  This is the engine uptake several open reports were explicitly waiting on.
  0.3.6/0.3.7 carried the FTS5 open-path hydration and lazy-lifecycle work
  (frankensqlite#358/#359), but cass could not move off `=0.3.4` because
  `fsqlite-ext-json` 0.3.6+ enabled serde_json's viral `arbitrary_precision`
  feature (frankensqlite#375), which breaks float fields in cass's tagged
  enums. 0.3.8 puts that behind an opt-in gate, so this is the first cass
  release able to carry the hydration fixes. Issues gated on this uptake and
  now eligible for re-measurement:
  [#320](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/320),
  [#349](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/349),
  [#379](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/379),
  [#381](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/381),
  [#382](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/382),
  [#390](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/390),
  [#398](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/398),
  [#401](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/401),
  [#402](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/402),
  [#406](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/406).
  Re-measurement is what closes them; the pin alone is not evidence.

- **Lexical backend flipped from Tantivy to Quill; a one-time `cass index
  --full` is required after upgrading** ([`ac2fe916`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ac2fe916),
  follow-ups through [#400](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/400)).
  The lexical index is keyed by its schema generation (`index/<generation>/`),
  and the flip bumped that generation to `v9-quill`, a sibling of the
  Tantivy-era `v8` directory. An archive indexed by v0.6.25 or earlier is
  therefore reported by `cass status` as `Not found - run 'cass index --full'`
  — not stale, not corrupt — until it is rebuilt once. The SQLite archive,
  analytics rollups and semantic vectors are untouched by the rebuild, and the
  old `v8` directory is left in place until the next schema-mismatch wipe.
  Interrupted rebuilds now resume rather than restart from zero, and the
  staged Tantivy merge path that large-archive rebuilds stalled in
  (#400) no longer exists.

### Fixed

- **`main` did not build: the committed `Cargo.lock` resolved `fsqlite-error`
  at `0.3.9` while `build.rs` requires a single `0.3.8` engine family**, so
  every build aborted with `dependency source contract violation`. Only
  `fsqlite` and `fsqlite-types` carry `=` exact pins; the other 18 family
  members ride caret ranges, so a bare `cargo update` can float one member
  off-pin. Repinned with `cargo update -p fsqlite-error --precise 0.3.8`;
  all 20 family members now resolve uniformly at 0.3.8. The check in
  `build.rs` was deliberately left as-is rather than relaxed.

- **`analytics rebuild --since`/`--days` were parsed and silently ignored;
  every run was a full rescan with quadratic pagination and no progress**
  ([#412](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/412)).
  The rebuild now honors `--since <date|-Nd>` / `--days N`: the window is
  widened to the start of the UTC day containing the cutoff, rollup rows for
  those days are dropped, and only messages on or after that day start are
  rescanned — older rollups are left intact. Pagination is keyset
  (`WHERE id > last_id`) instead of `LIMIT/OFFSET`, so a full rebuild is
  linear in the archive instead of re-reading every preceding row per chunk.
  Each 10k-message chunk logs an `analytics_rebuild_progress` event
  (processed/total, rate) so a long run is distinguishable from a hang.
  `--until`, `--agent`, `--workspace` and `--source` are query-time filters a
  rebuild cannot honor and are now rejected with a usage error instead of
  being accepted and ignored; `--since`/`--days` do not apply to Track B
  (`token_daily_stats`), which says so on stderr and rebuilds fully.
- **`--source remote` returned nothing on the lexical lane; `--source local`
  excluded named local-kind sources** (bead 5bf29). The Quill backend's
  `Remote` clause matches the `origin_kind` term `ssh` while cass indexes
  remote provenance as `remote`, so after the Tantivy→Quill flip a remote
  filter could never match a cass document. Remote selection now happens on
  hydrated hits through the same over-fetch/retry path `--sessions-from`
  uses, and `local`/`remote` select by origin *kind* on every lane (lexical,
  SQLite fallback, semantic filter maps), so a configured local-kind source
  such as a backup root or `chatgpt-import` is local. Analytics and
  incident-discovery filters still classify by id.
- **Stale or mid-backfill vector assets are no longer consulted by search**
  ([#404](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/404)).
  The query path admitted any FSVI/HNSW artifact that parsed, so a default
  (hybrid) search against a vector index built for an older database returned
  `k` arbitrary nearest neighbours for an out-of-vocabulary term while
  `cass status` reported `can_search:false` / `fallback_mode:"lexical"`.
  Semantic context loading now applies the same manifest-vs-live-DB readiness
  verdict status uses (computed on the DB handle it already opens, no second
  archive open): hybrid fails open to lexical with the reason in `_meta`,
  explicit `--mode semantic` returns exit 15. New
  `SemanticAvailability::IndexStale`. The response schema now documents that
  `total_matches` is a page-window lower bound unless
  `_meta.pagination.count_precision` is `exact`.
- **`cass import chatgpt <path> --json` is accepted** (previously only the
  leading `cass --json import …` form worked; the trailing flag was a clap
  error). `--robot` is an alias, matching the other subcommands.
- **Explicit `--mode semantic` no longer runs lexical self-heal first**
  ([#407](https://github.com/Dicklesworthstone/coding_agent_session_search/pull/407)
  idea, reimplemented). A semantic-only request never fails open to lexical,
  so repairing or lock-waiting on a stale lexical checkpoint before reaching
  HNSW was pure latency; hybrid/lexical requests keep the repair path.
- **Doctor's `>256 MiB` full-page integrity deferral now names its override**
  ([#390](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/390)).
  `CASS_DOCTOR_FULL_PAGE_INTEGRITY_PROBE=1` lifts the byte cap (the probe
  still runs under `CASS_DOCTOR_DB_PROBE_TIMEOUT_SECS`), so operators on
  long-lived archives can run the full frankensqlite `integrity_check`
  through doctor instead of only stock `sqlite3`.
- **Serial persist chunks retry transient engine contention**
  ([#401](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/401),
  [#406](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/406)).
  A single `BusySnapshot`/`WriteConflict` on one batch was surfaced as a fatal
  `exit 7` that discarded the run; the serial writer path now reuses the
  existing jittered bounded retry (8 attempts, ~1.5 s) before giving up.
- **`cass analytics rebuild --track b` checkpoints and resumes**
  ([#386](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/386)).
  `rebuild_token_daily_stats` was one transaction with in-memory cursors, so
  every interruption restarted the message scan from zero. It now aggregates
  into a persistent `token_daily_stats_rebuild_stage` table, commits every
  1000-conversation batch together with a `meta` cursor (ledger fingerprint +
  last conversation id), resumes from that cursor when the `token_usage`
  ledger is unchanged, swaps stage→live atomically at the end, and drops the
  scratch table so a finished rebuild leaves the archive schema untouched.
- **`cass import chatgpt` output is now indexed on Linux and Windows**
  ([#378](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/378)).
  The connector's only default root is the macOS app-support dir and the
  indexer scans default roots only for detected agents, so imported files sat
  in a directory nothing scanned. Import now registers its resolved output
  directory as an explicit local source (`chatgpt-import`) in `sources.toml`
  (on non-macOS, or whenever `--output-dir` is given); explicit roots are
  scanned regardless of detection. `--json` output carries `scan_root`.
  Output filenames are now derived safely: an export `id` is used verbatim
  only when it is a plain token, otherwise (path separators, whitespace,
  missing id) the file is named by a BLAKE3 digest of the conversation —
  content-keyed rather than positional, so a re-export in a different order
  no longer silently skips different conversations. Covered end to end by
  `tests/e2e_import_chatgpt.rs` (import → index → search through the binary).
- **Daemon fallback is no longer silent**
  ([#409](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/409)).
  At the pinned frankensearch rev the legacy `DaemonFallbackEmbedder`
  constructor rejects every caller-supplied daemon identity, so a reachable
  warm daemon was bypassed for full in-process inference with only a
  debug-level trace. cass now prints a stderr warning naming the cause
  whenever a reachable daemon is bypassed. The producer-attested daemon
  protocol itself is tracked as follow-up work.
- **`--db` now isolates the whole sandbox, not just the SQLite file**
  ([#403](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/403)).
  When `--db` points outside the default data dir, derived assets — lexical
  index, raw-mirror, checkpoints, `index-run.lock` — resolve to the db file's
  parent directory instead of silently reading and writing the LIVE install's
  data dir (which previously let a throwaway `--db` rebuild overwrite the live
  lexical checkpoint fingerprint and contend for live locks). An explicit
  `--data-dir` still wins; `--db` inside the default data dir keeps today's
  behavior.
- **`cass health` no longer reports `db=available` / "Search usable now: yes"
  on an unopenable archive** ([#396](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/396)).
  The health fast surface still elides busy/lock-class open failures (a
  concurrent writer is not a degraded-state signal), but hard failures —
  CANTOPEN, corrupt header, permissions — now surface as an open error, flip
  the readiness class to `db=open-failed`, and exit unhealthy, matching what
  `cass search` actually experiences.

## [v0.6.25] -- 2026-08-14

**The FrankenSQLite 0.3.1 engine wave plus the post-v0.6.24 GitHub-issue
triage fixes. The whole franken dependency family moves in lockstep: fsqlite
`=0.3.1` (crates.io), asupersync `=0.4.4`, frankensearch remote head
`14d1480a` (tantivy 0.27), and franken-agent-detection `57d2789e`.**

### Changed

- **FrankenSQLite `=0.2.1` → `=0.3.1`.** Carries the asupersync-0.4.3 runtime
  migration, the 0.3.0 fix wave (GH#333 concurrent-open BusyRecovery retries;
  GH#334 `FileIdentity` re-derivation after file replacement — including the
  cass#393 macOS post-reboot `st_dev` namespace-sidecar repair; the 8-writer
  rollback cascade; cross-connection visibility), and the 0.3.1 correctness
  wave (allocator post-savepoint page-aliasing quarantine, committed-freelist
  resurrection refusal, shared concurrent-writer EOF high-water, sidecar-less
  read-only first-contact opens [fsqlite GH#140], Darwin OFD locking).
- **asupersync `=0.3.10` → `=0.4.4`** (fsqlite 0.3.x requires the 0.4.x line;
  the 0.3.x and 0.4.x asupersync type universes are non-interchangeable.
  0.4.4 preserves a spawned task's typed result across cancellation
  acknowledgement instead of letting a concurrent abort erase it).
- **frankensearch pinned to remote head `14d1480a`** (491 commits past the
  previous `fbde8022` pin; brings tantivy 0.27). cass's segment-assembly path
  was ported off the removed `SegmentMeta::list_files()` API and the
  progressive-lexical adapter's `doc_count` now returns `Result`.
- **franken-agent-detection `88fc6783` → `57d2789e`** (fsqlite 0.3.x +
  asupersync 0.4.3 lockstep bump; 1004 tests green on the new stack).

### Fixed

- **#394**: one-shot `cass index --semantic` no longer re-embeds the entire
  corpus when the embedding watermark already covers the newest message and a
  vector index exists (the skip was previously gated on watch mode only).
- **#392**: `cass sources sync` now tells the truth when transfers fail:
  top-level `status` is `complete`/`partial`/`failed` (with
  `sources_attempted`/`sources_with_failures`/`sources_fully_failed` counts),
  exit 12 when every attempted source failed all paths, exit 8 on partial
  failure.
- **#377**: watch-once paths are absolutized and symlink-resolved at resolve
  time, so relative or symlinked inputs match connector scan roots instead of
  silently producing a skipped run; uncanonicalizable paths warn loudly.
- **#383** (partial): the semantic-resume "has message after cursor" probe now
  answers from `conversation_tail_state` plus one exact `(conversation_id,
  idx)` point lookup instead of walking conversation histories, with fallback
  to the exhaustive probe when tail state is absent.
- **#397**: the TUI no longer triggers a full analytics auto-rebuild on every
  launch for corpora with no API token metadata — an empty `token_usage`
  ledger with an empty `token_daily_stats` rollup is now a terminal fresh
  state for Track B. And `cass analytics rebuild --track a|b|all` now exists,
  so the `--track all` action that `analytics validate` has been recommending
  is finally runnable (Track B rebuilds route through
  `rebuild_token_daily_stats`).

## [v0.6.24] -- 2026-08-11

**Everything in the 98 commits since v0.6.23: the post-v0.6.23 GitHub-issue
triage wave documented below, plus the frankensqlite dependency re-pin that
turns two of those entries from "pending" into shipped, doctor's
corrupt-FTS5-shadow repair wiring, wall-clock budgets across index/search,
and status/health truthfulness under repair. Field-validated the day of
release on a production incident: a 9.3 GB corpus whose v0.6.23 binary
busy-spun indefinitely on open now gets a bounded 30 s open refusal, an
honest 27 ms `cass health` state report during rebuild, and a working
set-aside + full-reindex recovery (sub-second search afterward).**

### Fixed (release-gate wave, 2026-08-11 -- bead t8gor)

The v0.6.24 release gate itself caught three cross-platform defects in the
storage repair/recovery paths plus a darwin-only failure family; all are
fixed in this release:

- **Lexical-rebuild planner no longer under-plans on a stale-low tail
  cache** (`70713db0`): footprints are raised from `MAX(idx)+1` via the
  covering index, so a stale-low `conversation_tail_state` cannot trip the
  doc>plan invariant (the 0.6.23 anti-wedge restriction is preserved for
  the mixed-coverage path).
- **Legacy duplicate `fts_messages` schema rows no longer lose FTS content
  on rebuild** (`66c3add6`): when the surviving DDL is not cass's canonical
  `content=''` registration, the rebuild routes to drop/recreate (which
  collapses every same-named catalog row) instead of delete-all -- closing
  the contentless-write/content-hydrated-read split at the engine's
  rootpage-zero dedup exception.
- **`seed_canonical` restore works under the engine's fail-closed
  namespace semantics** (`70713db0`): after promoting a staged bundle, one
  writable open republishes the namespace identity for the promoted bytes
  (the bundle move deliberately does not carry `-fsqlite-ns-*` sidecars).
- **darwin: semantic-manifest publishing works under symlinked temp roots**
  (`f823aa11`): path link-checks anchor at the trusted data root instead of
  walking from the filesystem root (macOS `/var` symlinks to `private/var`
  and broke every publish); in-tree symlink rejection is unchanged.
- **darwin: transient Busy on connection close no longer fails persists**
  (`b4d9ce05`, `466faf05`): frankensqlite close retries Busy-class errors
  with backoff (close-time commit/checkpoint does not honor the busy
  timeout at the pinned rev), and the test env-lock recovers from poison.

Post-release note: fsqlite 0.2.1 (crates.io, 2026-08-11) now contains this
release's pinned engine line plus the GH#294 mutation-free opens; migrating
off the git pin to the registry is the next release's first unit.

### Changed (dependency re-pin and repair wiring, post-triage)

- **frankensqlite is now pinned to git rev `2351c6c5`** (`12129e25`,
  budgets/pin wave `f6cd6376`): the shipped binary now actually contains the
  multi-leaf FTS5 segment writer for #369 and the deferred-FTS5-validation
  open mode for #368 defect 3 -- both previously noted below as "pending a
  dependency re-pin" -- plus the 2026-07-21 namespace-lifecycle durability
  wave (`ReadOnlyExisting`).
- **doctor can rebuild a structurally-corrupt FTS5 shadow that blocks its own
  open** (#368 defect 3, `0ae97aaa`, e2e-covered `8cc94f78`), riding the
  deferred-validation open above.
- **Wall-clock budgets for index/search plus storage hardening**
  (`f6cd6376`); **large-archive rebuild, search budget, and status freshness
  fixes** (`fd78c5af`); **FSVI opened read-only with fail-closed two-tier
  sync** (`ee8b585d`).
- **Long connector scans tick activity via ScanContext progress-tick**
  (#373 Variant A, `3523bf20`).
- **Redaction memo-cache performance rounds** (`f5f4d7ba`, `e4a15698`,
  `ec20b699`): prefilter-first bypass, O(log n) LRU, and a 64 KiB memo input
  cap bounding cache memory.

Post-v0.6.23 GitHub-issue triage: the second half of the
[#368](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/368)
FTS5 corruption-opacity defects, three fresh v0.6.x reports, and the genuine
engine fix for the shipped
[#369](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/369)
FTS5 leaf-overflow (implemented + tested in frankensqlite, pending a dependency
re-pin).

### Fixed

- **A stale-low tail cache no longer under-plans the lexical shards.** The
  0.6.24 wedge fix stopped running the exact-count pass whenever every
  conversation already had tail coverage. That pass was also the only thing
  catching a tail cache that *understates* reality. Both
  `conversations.last_message_idx` and `conversation_tail_state` are caches,
  and when both go stale-low the planner sized shards from the stale number
  and tripped the `doc>plan` invariant mid-rebuild. Footprints are now raised
  from `MAX(idx)` read through the covering index
  `idx_messages_conv_idx(conversation_id, idx)`. That is a per-conversation
  seek, and the same query the missing-tail path already ran, so the multi-GB
  prepare wedge stays fixed; the expensive unindexed `COUNT(*) ... GROUP BY`
  that caused the wedge stays restricted to the mixed-coverage case.
  `MAX(idx)+1` over-estimates when idx values are sparse, which is the safe
  direction, since over-planning costs shards while under-planning corrupts
  the rebuild. The release test gate caught this as a regression against
  v0.6.23.

- **[#368](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/368)
  defect 2 -- readiness no longer reports "search usable" while search hard-fails
  on a corrupt FTS structure**
  ([`9bce58f4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9bce58f4),
  headline accuracy
  [`49efd299`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/49efd299)).
  A non-quarantine asset-inspection error (the lexical-fingerprint open failing
  on a corrupt FTS5 structure) fell through the `fresh=false ->
  StaleButSearchable` mapping, and the fingerprint open was skipped entirely for
  DBs > 256 MiB. Both fixed at the shared root, so `status`/`doctor`/`support-bundle`
  are all truthful; non-regressing for the absent-shadow (Tantivy-served) case.
  3 of 4 #368 defects now resolved (defect 3 is frankensqlite-blocked).

- **[#350](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/350)
  -- targeted `--watch-once <paths>` bounds to its paths instead of scanning the
  whole archive**
  ([`64b73dca`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/64b73dca)).
  A populated archive whose derived index looked stale silently broadened a
  targeted watch-once into a full-corpus rebuild (`processed_conversations=16301`
  for one changed session). The gate now bounds work to the supplied paths and
  defers any broader rebuild, as the plain-index path does.

- **[#353](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/353)
  -- the lexical-repair forever-defer loop is broken**
  ([`47fb0724`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/47fb0724)).
  Search deferred on a content-fingerprint mismatch while `cass index`
  re-preserved the stale checkpoint fingerprint above the 1 GiB auto-repair cap
  -- repeating every run. The deferred branch now runs a cheap, doc-count-gated
  metadata refresh that advances the fingerprint only when the live Tantivy index
  is provably equivalent (breaking the loop) and safely no-ops otherwise (never
  masking real staleness).

- **[#369](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/369)
  -- clearer doctor diagnostic for the oversized-leaf FTS shadow**
  ([`99fb72d4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/99fb72d4)),
  and the genuine engine fix (a multi-leaf FTS5 segment writer that partitions an
  oversized flush across leaves) implemented + tested in frankensqlite (branch
  `fts5-multileaf-cass369`, commit `30d31d51`), pending a dependency re-pin.

### Added (connectors)

- **Goose sessions are now indexed**
  ([`1967c7aa`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1967c7aa)).
  Block's Goose agent writes to `~/.local/share/goose/sessions/sessions.db`
  (SQLite) from v1.20 onward, and to per-session `*.jsonl` files under
  `~/.goose/sessions` before that. Both layouts are read through
  `franken_agent_detection`. This brings the connector count to 24;
  `cass capabilities --json | jq .connectors` remains the canonical inventory.

### Added (observability)

- **Half-built canonical FTS shadow surfaced in `status`/`index --json`** (zn1xn
  F5,
  [`ee39e37f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ee39e37f)):
  a skipped/failed derived fallback-FTS repair records a durable marker exposed
  additively under `/index/fallback_fts_repair`, so agents see a half-rebuilt
  shadow immediately rather than only when doctor's next parity check catches it.

- **Persistent lexical-repair deferral streak surfaced** (zn1xn F4,
  [`3ac90b85`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3ac90b85)):
  a consecutive-deferral counter + reason under `/index/lexical_repair_deferred`,
  exposing the recurring full-rebuild amplification that was previously only a
  stderr warn.

- **Memory-pressure attribution in ingest poison records**
  ([#364](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/364),
  [`83346cc4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/83346cc4)):
  a quarantine now records whether it was genuine host pressure vs a
  precautionary large-conversation quarantine.

### Investigated / tracked

- **[#345](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/345)**
  (FTS dry-run content faults) and
  **[#349](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/349)**
  (base-schema migration OOM on legacy internal-content `fts_messages`) are
  documented with precise findings: both need frankensqlite enhancements
  (index-only rowid projection; bounded WITHOUT-ROWID teardown) -- the same class
  as #368 defect 3. #364's chunked single-conversation ingest core (a perf
  optimization; the conversation is already indexed via a deferred rebuild) has a
  full implementation design on its tracking bead.

---

## [v0.6.23] -- 2026-07-30

**Everything landed in the 128 commits between the [v0.6.22 GitHub
Release](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.6.22)
(2026-07-06) and 2026-07-30: a hybrid semantic search subsystem plus
operations/incident observability, two connector expansions (Grok Build;
current Kimi Code + Oh My Pi), a late-July GitHub-issue triage wave (37
issues closed in the window), the first half of the
[#368](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/368)
FTS5 corruption-opacity defects, and a pinned-dependency wave
(frankensqlite `62a58ee3` hotfix, franken-agent-detection `1557300b`,
frankensearch `ad8e29ea`, asupersync `=0.3.9`). Shipping this as a release
binary also unblocks
[#352](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/352),
which was open only because the resumable fast-backfill fix (#348) existed
solely on `main`. 61 tracked workstreams were closed in
`.beads/issues.jsonl` over the same window.**

### Added

- **Grok Build (xAI's official `grok` coding CLI) is now a covered connector**
  (#328). Detection was already scaffolded; the parser + factory land via
  `franken-agent-detection` 0.1.10 (`6d24c532`). Sessions under
  `$GROK_HOME/sessions/<percent-encoded-cwd>/<session-uuid>/` are read from
  `updates.jsonl` — the ACP session-update stream the CLI's own docs call the
  authoritative conversation log — with `summary.json` supplying title,
  workspace (`info.cwd`), timestamps, and model, and `chat_history.jsonl`
  serving as a fallback so real user prompts still index when every model
  call in a session failed. Streaming chunk kinds coalesce, `tool_call` /
  `tool_call_update` events become tool messages with structured
  invocations, and unknown update kinds are skipped tolerantly. Registered
  end-to-end: connector factory, watch-root `ConnectorKind` (`gk`), source
  discovery/raw-mirror coverage, and the `capabilities --json` inventory
  (now 23 connectors).

- **Current Kimi Code layout + Oh My Pi coverage**
  ([#351](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/351);
  commits
  [`71aefb7e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/71aefb7e),
  [`8c01f50c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8c01f50c)).
  The Kimi connector now reads the modern `$KIMI_CODE_HOME` (default
  `~/.kimi-code`) tree — `sessions/<workDirKey>/<sessionId>/state.json`
  plus per-agent `agents/<agentId>/wire.jsonl` transcripts with the new
  event envelope (prompt-echo dedup, `tool.result` folded onto the
  originating call, collision-free `<sessionId>:<agentId>` sub-agent IDs) —
  while legacy `~/.kimi` behavior is preserved verbatim. Oh My Pi (`omp`)
  surfaces in the pi_agent connector: `.omp/agent` probe roots plus
  per-session sub-agent transcripts. Fresh-eyes hardening in the final
  `1557300b` pin: canonical scan-root dedup for symlinked `~/.kimi` →
  `~/.kimi-code`, `KIMI_CODE_HOME` override replaces (not prepends) the
  default roots, and out-of-order/duplicate tool results surface as
  standalone tool messages instead of dropping.

- **Lightweight semantic + hybrid search subsystem with trust-scored
  reranking** (commit
  [`31f8c14b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/31f8c14b)).
  Embedders behind an `embedder_registry` (FNV-1a feature hashing as an
  explicit degraded mode, pure-Rust native MiniLM), pure-Rust cross-encoder
  reranking with RRF-style fusion, and trust scoring/correlation weighting
  results, with storage-integrity and contention diagnostics keeping the
  vector path safe.

- **Analytics, operations dashboard, and guided-ops observability** (commit
  [`ebc181ae`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ebc181ae)).
  Operator-facing analytics (derive/query/types), the operations dashboard
  and pages bundle/wizard, and the daemon/indexer/storage plumbing behind
  them, gated by e2e contracts (onboarding, lessons, guided-ops golden,
  operations-dashboard contract, bounded quarantine-retry, robot-json).

- **Incident discovery, classification, and redaction for support bundles**
  (commits
  [`48c5ee78`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/48c5ee78),
  [`3699aca3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3699aca3),
  [`ee78f33d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ee78f33d)).
  Raw session evidence is classified into incident categories and passed
  through a redaction policy/manifest before it leaves the process, archive
  discovery and replay are hard-bounded, and the top incident-bearing
  sessions are summarized for the bundle.

- **Immutable, schema-versioned semantic generation manifests with atomic
  pointer publish** (commit
  [`7009b076`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7009b076)).
  Generations persist under `generations/<id>/` and are never mutated after
  publish; `current.json` swaps only after the target generation's manifest
  validates, so a restarted consumer resolves exact, digest-verified
  artifacts instead of conventional vector filenames — and serving consumes
  the exact FSVI artifacts (commit
  [`e6198947`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e6198947)).

- **Additive budget-envelope metadata on robot payloads** (commit
  [`e1ad3575`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e1ad3575)):
  optional top-level `budget` accounting (token/row/time) that existing
  consumers ignore and budget-aware clients read.

- **`cass sessions --since`, and crash-orphaned lexical staging dirs are
  reclaimed at startup**
  ([#324](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/324);
  commit
  [`4fddb21f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4fddb21f)
  — one crash loop had leaked 410 GB of `cass-lexical-*` staging overnight).
  `cass sessions --limit` also stays off the full-scan path via an
  `INDEXED BY` hint
  ([#323](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/323);
  commit
  [`46ccb036`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/46ccb036)).

- **`cass doctor` full-verify mode for the raw-mirror integrity check**
  (commit
  [`d6c2093e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d6c2093e)),
  and **encrypted-archive KDF parameter / key-slot validation on decrypt**
  (commit
  [`5a679b02`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5a679b02)).

### Changed

- **FrankenSQLite is pinned to the FTS5 overlong-term hotfix branch**
  `fts5-overlong-hotfix-cass362` @ `62a58ee3` (`0.1.19`), closing
  [#362](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/362)
  (commit
  [`19b664d2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/19b664d2)):
  terms over 1024 bytes are skipped at a shared capped tokenizer factory —
  a 91,548-byte token (normal data in coding-agent corpora) previously
  corrupted the `%_data` segment write and blocked every repair path,
  including from-scratch rebuilds; the porter stemmer passes >64-byte
  tokens through unstemmed; and a failed pending-flush is a hard error
  instead of silently writing an empty structure row. Pin journey inside
  this window: `=0.1.13` → `=0.1.18` (commit
  [`aa92a453`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aa92a453))
  → crates.io family (commit
  [`3abf6165`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3abf6165))
  → the git hotfix pin, because upstream main has moved to an async-first
  API and the full forward bump is a separate validated pass.

- **franken-agent-detection advanced to `1557300b`** (0.1.10): `6d24c532`
  (Grok Build parser/factory, commit
  [`aa92a453`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aa92a453))
  → `f7f38440` (OpenCode remote-root scan isolation, so local
  `opencode.db` sessions are no longer double-tagged under configured
  remote SSH sources —
  [#357](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/357);
  commit
  [`e2151f95`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e2151f95))
  → `1a258873` (Kimi Code / Oh My Pi, commit
  [`71aefb7e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/71aefb7e))
  → `1557300b` (fresh-eyes hardening, commit
  [`8c01f50c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8c01f50c)).

- **frankensearch pinned to `ad8e29ea`** with the explicit
  `lexical_tantivy` CASS compatibility surface and cancellation-safe facade
  opening (commits
  [`e6198947`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e6198947),
  [`b976d49d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b976d49d)
  — the latter also enforces exact dependency and daemon identity
  contracts).

- **asupersync `=0.3.5` → `=0.3.9`** (commits
  [`047f70a4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/047f70a4),
  [`aa92a453`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aa92a453)).

- **Release engineering**: simplified per-target release matrix and
  installer version/artifact handling (commit
  [`f83da705`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f83da705));
  the fresh-clone CI guard rejects only sibling-path `[patch]` overrides,
  so the checked-in hotfix pins build from a clean clone
  ([#361](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/361) /
  [#365](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/365);
  commit
  [`472983c2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/472983c2)).

### Fixed

- **`cass forget --robot` and `cass upgrade` parse again, and no longer sit
  in the typo-attractor net**
  ([#367](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/367);
  commits
  [`d62bb6a5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d62bb6a5),
  [`d887ae20`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d887ae20)).
  `forget`/`upgrade` joined `CANONICAL_TOP_LEVEL_COMMANDS` — the list is
  now pinned to the clap tree in both directions — and side-effect commands
  are matched exactly only: the Levenshtein-2 net had made `cass format`
  hard-fail on `forget`'s required `--source-glob` and made `cass parade`
  silently *execute* the upgrade release-check.

- **cass dies quietly on stdout EPIPE**
  ([#356](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/356);
  commit
  [`87a39572`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/87a39572))
  via the default SIGPIPE disposition — an early-exiting pipe consumer no
  longer produces a SIGABRT and a macOS crash report.

- **Watch-once no longer quarantines a session on a false OOM**
  ([#364](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/364),
  the quarantine-poisoning half; commit
  [`ef7e2595`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ef7e2595)):
  an ingest `NoMem` while the host reports ample memory now defers instead
  of quarantining the session. The issue stays open for its remaining half.

- **Stall-watchdog liveness across the ingest → staged-shard-build
  hand-off**
  ([#366](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/366)
  and the watchdog half of
  [#359](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/359);
  mitigates #329/#345; commits
  [`368467d6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/368467d6),
  [`8b42750a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b42750a)).
  `cass index --full --force-rebuild` on a large corpus no longer aborts
  (exit 70) in a legitimate hand-off window; park-site flapping between
  waiting states no longer resets the stall clock; and an optional FTS
  repair failure no longer fails a completed `index --full`.

- **Truthful deferral verdicts for large archives**
  ([#359](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/359);
  commit
  [`5fb24973`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5fb24973)).
  Plain `cass index` now states when it deferred the authoritative lexical
  rebuild and names `cass index --full` as the completing command, and the
  `checkpoint_incomplete` hint is size-aware instead of recommending the
  exact command that defers again. "index completed" is also no longer
  printed after a failed run (commit
  [`8b42750a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b42750a)).

- **`cass sources add` merges `--preset` with explicit `--path` instead of
  silently dropping the custom paths**
  ([#358](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/358)),
  and **doctor flags a queryable-but-partial canonical FTS shadow
  (PartialParity) instead of passing it**
  ([#355](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/355))
  — commit
  [`2c85eeaf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2c85eeaf),
  with the parity warning noting that small drift during an active index
  run is transient (commit
  [`8b42750a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8b42750a)).

- **First half of the #368 FTS5-corruption failure chain**
  ([#368](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/368),
  defects 1 and 4 of 4; commit
  [`d887ae20`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d887ae20)).
  Every fingerprint-failure render site now formats the full `anyhow` chain
  with `{err:#}`, so the root cause (`fts5: corrupt %_data record ...`) is
  no longer swallowed by the outer "opening readonly storage" context. A
  `doctor --fix` that crashed mid-repair now self-heals its stale
  mutation-lock state — evidence-gated (flock acquirable *and* heartbeat
  beyond a 24 h bound), markers moved (never deleted) into
  `doctor/reclaimed/<ts>-stale-repair-state/` with a `receipt.json` — where
  it previously left a 9-day dead end with no way out. And the new
  `DoctorFtsTableState::ShadowCorrupt` classification warns on a queryable
  `fts_messages` whose docsize shadow cannot be counted instead of passing
  with the error buried. Defects 2–3 remain open.

- **Resumed semantic candidate selection remains bounded across sparse parent
  gaps** (#348). Resume scans now stream eligible rows once from the canonical
  global message cursor and retain only the smallest bounded set of
  conversation IDs. This removes thousands of per-parent probes (and their
  multi-gigabyte allocator growth) while preserving exact ascending
  conversation checkpoint order even when message IDs arrive out of order.
  A 6,668-parent-gap regression proves bounded row visits, memory, and time.

- **Semantic CLI searches discover an already-running embedding daemon by
  default without eagerly loading a local model** (#347). Explicit `--daemon`
  additionally permits auto-spawn, while `--no-daemon` keeps direct local
  inference. The local quality embedder is lazy and only initializes on daemon
  fallback. Every daemon response must identify the embedder that owns the
  active vector index, and fast-only mode is pinned to the hash vector space,
  so a same-width but incompatible vector can never silently contaminate an
  index.

- **Large-archive doctor inventory and raw-mirror accounting are streaming and
  bounded** (#330). Source inventory aggregation now happens in bounded Rust
  maps over one-row metadata streams instead of a FrankenSQLite
  `GROUP BY`/`ORDER BY` materialization. Raw-mirror message counts use one
  narrow composite-index stream (or one legacy table-scan fallback) instead of
  a correlated full message scan per conversation. Plan regressions forbid
  temporary sorters and verify exact counts on sparse fixtures.

- **`daily_stats` recovery no longer attempts an archive-sized packet rebuild
  before its nominal fallback** (#329). Production now has one authoritative,
  failure-atomic staging rebuild. Canonical messages are read through one
  prepared, row-streaming composite-index scan, with bounded pages, explicit
  phase/cursor diagnostics, retry shrinkage on memory pressure, and exact,
  idempotent publication. Packet projection remains test-only as an oracle.

- **Canonical FTS doctor repair now scales to multi-million-message archives**
  (#345). Exact dry-run parity is classified from canonical, indexed, and
  intersection cardinalities using fused composite-index parent-run counts and
  a single shadow-driven primary-key probe stream instead of repeatedly
  rescanning 4,096-row keyset pages or retaining the complete shadow rowid
  domain. The FrankenSQLite
  pin was advanced to 0.1.18 (superseded later in this same release by the
  `62a58ee3` hotfix pin — see *Changed*), whose fused composite-index equality-run
  counter supplies the bounded canonical count and whose bounded clean-page
  reclamation prevents the false `OutOfMemory` failure observed while creating
  an absent FTS shadow inside a 24 GiB cgroup. Failure-atomic publication and
  canonical-row preservation are unchanged.

- **`cass status --json` no longer synthesizes `storage_state: "ok"` from
  openability alone** (#331). When the only probe that ran is `db_open`, the
  lightweight readiness surfaces (`status --json`, `search --robot-meta`) now
  report the honest `storage_state: "unchecked"` / `source_of_truth_risk:
  "unknown"` with an explicit `checks_not_attempted: [quick_check]` entry.
  `ok` is reserved for real structural evidence: the doctor's completed
  integrity probe now persists a fingerprinted attestation
  (`integrity_attestation.json`, keyed on db+WAL size/mtime, 7-day TTL) that
  status projects with `attestation_source: "cached"` — including a cached
  FAIL, which projects `integrity_failed`/`high`, gates top-level
  `healthy: false`, and adds an explicit recovery warning, so a
  quick_check-provable corrupted archive can no longer read as healthy just
  because it still opens.

- **`cass index --full` no longer emits false `stall_detected` events during
  active parsing, and progress ETAs no longer regress as discovery expands
  the total** (#332). The stall watchdog now watches a monotonic
  work-liveness tick (`activity`: parsed conversations on the producer side,
  received/persisted batches and chunks on the consumer side) in addition to
  the published `current` counter, so a long parse of one large source
  artifact between coarse batch publications is forward progress, not a
  stall. Progress snapshots additionally carry `unit` (connectors /
  conversations / messages / vectors per phase) and `total_is_final`;
  `eta_seconds` is suppressed while `total` is still a lazily expanding
  "discovered so far" count instead of regressing by hours.

- **Semantic rebuilds are failure-atomic, and the index → embed hand-off no
  longer deadlocks (fastembed) or panics (hash)**
  ([#333](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/333);
  commit
  [`486801f9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/486801f9)
  — the same commit fixed the macOS daemon-socket refusal on the symlinked
  `/tmp` → `/private/tmp` parent,
  [#346](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/346)).
  Semantic rebuild progress is reported
  ([#342](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/342);
  commit
  [`93401d54`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/93401d54)),
  and resumed semantic selection is bounded
  ([#343](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/343);
  commit
  [`75e66a17`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/75e66a17),
  completed by the #348 work above).

- **Canonical FTS repair is resumable and failure-atomic**
  ([#344](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/344);
  commit
  [`77c16b07`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/77c16b07)).
  The same change gives `doctor --rebuild-canonical-fts --dry-run`
  unconditional precedence over `--yes` — the non-mutating preview that
  [#354](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/354)
  reported as missing.

- **Connector regressions**: Gemini CLI's current `session-*.jsonl`
  sessions ingest again
  ([#341](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/341);
  commit
  [`11900a44`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/11900a44));
  the Codex connector preserves modern `function_call` arguments in
  searchable content
  ([#339](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/339);
  commit
  [`8eb80495`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8eb80495));
  mutable watch-once semantic artifacts reconcile
  ([#336](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/336);
  commit
  [`af95327b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/af95327b));
  guide planner aliases are disambiguated
  ([#337](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/337);
  commit
  [`84d71f25`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/84d71f25)).

- **Early-window indexer reliability**: the post-publish WAL checkpoint is
  no longer killed as a finalize wedge
  ([#319](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/319) /
  [#321](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/321);
  commit
  [`e4360670`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e4360670));
  a corrupt derived fallback-FTS after a successful full index run is
  non-fatal
  ([#327](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/327);
  commit
  [`8110944f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8110944f));
  deferred-checkpoint mode uses the bounded bulk-import WAL cadence instead
  of disabling autocheckpoint (commit
  [`968a5f3c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/968a5f3c)).

- **Storage/doctor/fleet batch (2026-07-22)**: embedder verification,
  quality-eval sanitization, and FTS5 duplicate recovery (commit
  [`78d25b7f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/78d25b7f));
  the daemon reconciles the semantic index with canonical messages (commit
  [`ada71e79`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ada71e79));
  `daily_stats` rebuild stays bounded (commit
  [`51ca7e9f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/51ca7e9f));
  sources-doctor assesses local storage without mutating the source path
  (commit
  [`34676c12`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/34676c12))
  and prefers preservation evidence over the generic unreadable-path signal
  (commit
  [`a5edfe67`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a5edfe67));
  fleet doctor state stays truthful (commit
  [`52e03506`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/52e03506))
  and unsuccessful upgrade rehearsals fail (commit
  [`28c92ca1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/28c92ca1));
  repro emits a runnable, share-safe rerun contract (commit
  [`82927dfc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/82927dfc)).

- **Test-infrastructure and pinned-dependency fallout**: frankensqlite VFS
  namespace lock sidecars (`-fsqlite-ns-gate` / `-fsqlite-ns-use`) are
  never treated as historical bundles in salvage (commit
  [`d3adf9da`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d3adf9da))
  and are skipped by the archive-export / support-bundle manifest walkers
  and candidate promotion (commit
  [`d887ae20`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d887ae20));
  pre-existing lib-suite breakage was repaired so `main` is green (commit
  [`a37c9069`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a37c9069));
  introspect golden coverage is bound to the executable matrix (commit
  [`fdf28d22`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fdf28d22));
  compatibility and manifest proofs were hardened (commit
  [`9d02210c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9d02210c));
  and the E2E suite binds strict-RCH evidence to immutable runs (commits
  [`78ea1ecb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/78ea1ecb),
  [`29b87cd6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/29b87cd6)),
  isolates child-process environments (commit
  [`704a49f1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/704a49f1)),
  and bounds semantic trace artifacts (commit
  [`aefd6357`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aefd6357)).

> **Still open after this release**:
> [#329](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/329)
> (daily_stats RSS on a 967k-message corpus),
> [#345](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/345)
> (dry-run cost on a large partial shadow),
> [#349](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/349) /
> [#350](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/350)
> (macOS migration OOM / watch-once scan breadth),
> [#353](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/353)
> (status/search fingerprint disagreement — circular mechanism analyzed and
> an equivalence-gated repair designed on the tracker), the remaining half
> of
> [#364](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/364),
> and defects 2–3 of
> [#368](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/368).

## [v0.6.20] -- 2026-06-30

**Linux x86-64 reliability release: a pure-Rust embedder backend (no more
ONNX runtime), two indexer fixes for watch-mode and large-batch memory, the
`cass upgrade` installer-asset repair, and the modern-Codex transcript fix
via a `franken-agent-detection` bump.**

### Changed

- **Dropped the ONNX runtime / fastembed stack for a pure-Rust native
  embedder + reranker** (commit
  [`82547d35`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/82547d35), #308). The default MiniLM
  quality-tier embedder no longer depends on `onnxruntime`, which on Linux
  x86-64 (AVX2) failed with `LayerNormalization ... GetElementType is not
  implemented` and embedded 0 docs → stall-abort exit 70. A single binary now
  serves all x86_64 (the `-baseline` AVX split is obsolete).

### Fixed

- **`cass index --watch` deterministic exit-70 crash-loop** (commit
  [`345f1ccc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/345f1ccc), #311). The F4 stall
  watchdog's #297 finalize-wedge abort fired on a `--watch` daemon's normal
  quiescent idle (phase 0, `current >= total`, pipeline quiescent), exiting 70
  every ~300s. The watchdog is now watch-aware: that resting state is treated
  as healthy idle in watch mode, while genuine phase-2 wedges (and the oneshot
  post-publish wedge #297 was written for) are still detected and aborted.
- **`cass models backfill --tier quality` memory-blowup / CPU-spin** (commit
  [`345f1ccc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/345f1ccc), #309). Fixed-128-row
  embed batches padded every row to the longest member, so one long message
  inflated the padded tensor to multiple GB. Length-aware batching now caps
  both row count and `row_count × max_canonical_len` (default 16 KiB, env
  `CASS_SEMANTIC_EMBED_BATCH_CHAR_BUDGET`), order-preserving and embedder-agnostic.
- **`cass upgrade` 404** (#310). The v0.6.18/v0.6.19 GitHub Releases were
  missing the installer assets the self-updater fetches; they were restored
  (`install.sh`/`install.ps1` + `SHA256SUMS` rows) and verified end-to-end.
  Future releases go through the canonical `release.yml`, which attaches them.
- **Modern Codex transcripts** via **`franken-agent-detection` 0.1.8 → 0.1.9**
  (commit [`c60943e7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c60943e7), upstream #13). The
  Codex connector now captures modern `output_text` assistant messages and
  `response_item` tool calls/outputs that were previously dropped.

## [v0.6.9] -- 2026-05-30

**Two correctness fixes uncovered by a fresh-eyes review of the v0.6.7
watchdog: ARM memory-ordering soundness + lock-file write-race against
the heartbeat thread.**

### Fixed

- **ARM (AArch64) memory-ordering soundness for watchdog state
  observation** (commit [`f20e6497`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f20e6497), INV2). The v0.6.7
  `WatchStartupPreflightState::enter` wrote `current_step_idx` first
  (Relaxed) then `step_started_at_ms` (Relaxed). On ARM's
  weakly-ordered memory model — production targets
  `aarch64-unknown-linux-gnu` and `aarch64-apple-darwin` — the watchdog
  thread could observe these two Relaxed stores out of order: new
  `step_idx` with stale `step_started_at_ms == 0`, computing
  `elapsed_ms = now_ms - 0 ≈ 1.7×10¹² ms`, exceeding any timeout, and
  firing a spurious `_TIMEOUT` on the very first poll tick after step
  entry. Fix: write `step_started_at_ms` first (Relaxed), then
  `current_step_idx` with `Release` ordering; watchdog loads
  `current_step_idx` with `Acquire`. The Release-Acquire pair
  establishes happens-before so the subsequent `step_started_at_ms`
  load sees the value written before the Release store.

- **Watchdog `_TIMEOUT` breadcrumb no longer silently overwritten by
  the heartbeat thread** (same commit, INV3). The v0.6.7
  `rewrite_lock_phase_for_timeout` did NOT hold
  `metadata_write_lock` during its lock-file rewrite, so a heartbeat
  tick interleaving between the watchdog's `set_len(0)`/write and the
  process exit could overwrite the `_TIMEOUT` breadcrumb with the
  prior-phase content. Operators reading `cass health --json` after
  the abort would see no `_TIMEOUT` suffix, defeating the diagnostic
  feature. Fix: watchdog now acquires `metadata_write_lock` for the
  duration of the rewrite. Regression test:
  `watchdog_timeout_rewrite_serialised_by_metadata_write_lock`.

### Notes

- Same fresh-eyes-review meta-pattern as v0.6.5 (#256 partial fix) and
  v0.6.8 (cross-surface accumulator storm). Each pass keeps finding
  real bugs. Recommend continuing the review-pass discipline.
- v0.6.7 and v0.6.8 BOTH ship the ARM bug. v0.6.9 is recommended for
  all users; v0.6.7/v0.6.8 should be yanked from crates.io after
  v0.6.9 confirms green on the prebuilt-binary smoke tests.

## [v0.6.8] -- 2026-05-30

**Cross-surface retry-storm fix uncovered by a fresh-eyes review of the
v0.6.7 legacy quarantine retry (`is_version_stale_for_retry`).**

### Fixed

- **Stale-poison version accumulator no longer false-positives when one
  surface stamps `cass_version_at_quarantine = current_version` but the
  other surface's save fails** (commit [`7510d6c1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7510d6c1)). The v0.6.7 retry
  logic checked each surface independently — if `mark_stale_index_ingest_jsonl_retry_attempted`
  succeeded (stamped JSONL with current_version) but
  `mark_stale_index_ingest_structured_retry_attempted` failed at the
  structured-state save step (disk full / permissions / etc.), the next
  scan would see:
  - JSONL: `cass_version_at_quarantine == current_version` → no-op
  - Structured: `cass_version_at_quarantine == None` → "legacy, retry
    eligible"
  And retrigger a full quarantine scan every single run forever.

  Fix: `StalePoisonVersionAccumulator` gains an `already_current_keys:
  BTreeSet<(String, i64)>` cross-surface dedup. When ANY surface observes
  `cass_version == current_version` for a key, the key is added to
  `already_current_keys` and removed from `stale_keys`/`legacy_keys`.
  Order-independent: the final state is always "not stale" if any surface
  says current, regardless of observation order.

  Regression test:
  `cross_surface_current_version_suppresses_legacy_structured_entry`.

### Notes

- The bug shape is the exact same as the inert-#258-writer the FIRST
  fresh-eyes review caught (and as #256's partial-fix that the third
  review caught). Each fresh-eyes pass continues to yield real defects.

## [v0.6.7] -- 2026-05-30

**watch_startup wedge hardening + legacy quarantine retry. Closes [cass#258
ask #5](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/258) (legacy quarantine entries) and ships the user-facing defensive
infrastructure that v0.6.6 set up via the sub-phase taxonomy. Reporter of
[cass#265](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/265) can unblock today via `CASS_SKIP_PREFLIGHT_CLEANUP_ORPHAN_FK_ROWS=1`.**

### Fixed

- **Legacy v0.5.1-era `index-ingest-out-of-memory` quarantine entries are
  now retry-eligible** (commit [`e5898858`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e5898858)). Pre-v0.6.x quarantine
  entries have `attempt_count=1` but LACK the `cass_version_at_quarantine`
  field, so the v0.6.x retry gate silently skipped them — they remained
  quarantined forever even after the underlying v0.5.x ingest-OOM bug was
  fixed. Read-side fix: `QuarantineRecord::is_version_stale_for_retry`
  returns `true` for `None` (legacy entries pre-date the bug-fix the gate
  is gated on, so retry is the right default). Regression test:
  `legacy_entry_missing_cass_version_deserialises_and_is_retry_eligible`.

### Added

- **Per-op `watch_startup` preflight watchdog with skip env vars** (commit
  [`5348ff2a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5348ff2a), 733 insertions). Each of the 14 documented preflight sub-phases
  now arms a watchdog at entry (`state.enter(step_idx, now_ms)`) and
  disarms at exit (`state.exit()`). Default timeout
  `CASS_PREFLIGHT_OP_TIMEOUT_SECS=180` (clamp `[1, 3600]`). When the
  watchdog fires:
  - The lock file's `phase=` breadcrumb is rewritten to
    `watch_startup:<step>_TIMEOUT` (so operators see exactly which step
    wedged).
  - All other lock fields (pid, started_at_ms, db_path, mode, job_id,
    job_kind) are preserved verbatim.
  - The process exits with a clear error message.
- **Per-op skip env vars** for the four wedge-candidate operations
  (`CASS_SKIP_PREFLIGHT_CLEANUP_ORPHAN_FK_ROWS=1`,
  `CASS_SKIP_PREFLIGHT_VALIDATE_FTS_MESSAGES=1`,
  `CASS_SKIP_PREFLIGHT_COUNT_TOTAL_MESSAGES=1`,
  `CASS_SKIP_PREFLIGHT_PUBLISHED_INDEX_VALIDATE=1`). Operators on cass#265
  can set the relevant variable as a workaround while the underlying
  fsqlite issue is rooted out. The reporter's empirical evidence points
  most strongly at `cleanup_orphan_fk_rows`.
- Regression test
  `watch_startup_preflight_watchdog_fires_on_wedged_step` simulates a
  wedge by calling `state.enter` and never exiting; asserts within 750 ms
  that `state.tripped == true`, the lock-file `phase=` is rewritten to
  the `*_TIMEOUT` form, and all other fields are preserved.

### Recommended diagnostic workflow for cass#265

If you're hitting the `watch_startup` wedge:

1. Upgrade to v0.6.7.
2. Re-run `cass index --watch`.
3. If it still wedges, the watchdog will exit at +180s with the wedged
   step in the error message and in the lock file's `phase=` breadcrumb.
4. Set the corresponding `CASS_SKIP_PREFLIGHT_<NAME>=1` env var as a
   workaround.
5. Report the wedged step on [cass#265](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/265) so the underlying fsqlite issue
   can be narrowed.

### Notes

- v0.6.7 ships diagnostic infrastructure + workarounds, NOT a root-cause
  fix to the underlying fsqlite wedge. The root cause is most likely a
  multi-level B-tree forward-scan path in fsqlite that the
  `cleanup_orphan_fk_rows` SQL query triggers; the actual fix lives in
  frankensqlite and will land in a future fsqlite release + cass repin.

## [v0.6.6] -- 2026-05-29

**Investigation-cluster release for [cass#265](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/265) (`watch_startup` wedge persists). Adds the sub-phase
breadcrumb taxonomy needed to narrow the wedge down from "preflight"
(14 operations) to a specific step. Diagnostic-only; v0.6.7 ships the
operator-facing workarounds.**

### Added

- **`WATCH_STARTUP_SUB_PHASE_TAXONOMY`** — 14 documented preflight
  sub-phase strings (commit [`fad3f03d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fad3f03d)). Each preflight operation now
  calls `set_phase(WatchStartup::SubPhase::*)` so the on-disk lock
  file's `phase=` breadcrumb reflects which step is currently
  executing. Operator visibility into the 14-step preflight block
  (previously all reported as `phase=watch_startup`).
- Regression test
  `watch_startup_sub_phase_taxonomy_is_documented_and_stable` pins the
  14 strings as a public operator contract.
- Regression test `set_phase_writes_sub_phase_breadcrumb_and_bumps_progress`
  exercises the new `set_phase` writer through `acquire_index_run_lock`
  and asserts on-disk `phase=` updates, `mode=` invariance,
  strict-monotonic `last_progress_at_ms`, and atomic-mirror consistency.

### Notes

- v0.6.6 ships the diagnostic infrastructure only. The reporter of
  [cass#265](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/265)
  needed to re-run cass against their corpus and share the
  `phase=watch_startup:<step>` string at +150s. v0.6.7 (this is the
  v0.6.7 entry below — read up) supersedes the manual workflow with
  an automated watchdog that fails fast at the wedged step.

## [v0.6.5] -- 2026-05-28

**Definitive close-out of cass#256 via a feature-gated `semantic` build, plus the cass#258 follow-on liveness work that v0.6.4's `last_progress_at_ms` plumbing left half-done.**

### Pre-AVX2 Windows + Linux baseline binaries (cass#256 — fully closed)

v0.6.3 added `RUSTFLAGS=-C target-cpu=x86-64-v2` as a defense-in-depth measure for the Windows release codegen. v0.6.4's CHANGELOG correction acknowledged that this was *necessary but not sufficient*: the `fastembed` crate enables `ort-download-binaries-rustls-tls`, which links prebuilt Microsoft ONNX Runtime binaries that already carry AVX/AVX2/FMA-dispatched code. `RUSTFLAGS` only constrains `rustc`'s own codegen and cannot reach object code linked from a vendor prebuilt, so `cass --version` continued to die with `STATUS_ILLEGAL_INSTRUCTION` (`0xC000001D`) on Ivy Bridge hardware (confirmed by reporter @Dlows-Vibe on an i7-3770K). v0.6.5 closes this for good.

- **New `semantic` Cargo feature ([`d9b98126`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d9b98126)).** `fastembed` is now `optional = true`, and the `frankensqlite/fastembed-reranker` re-pull is gated behind the same flag. The umbrella feature is declared as `semantic = ["dep:fastembed", "frankensearch/fastembed-reranker"]` and is included in `default = ["qr", "encryption", "semantic"]`, so the default `cargo build` / `cargo install` path is byte-for-byte equivalent to v0.6.4 and existing users see no behavioural change. Disabling the feature (`--no-default-features --features qr,encryption`) drops the entire ONNX Runtime stack from the link line, including the AVX2-dispatched prebuilt objects.
- **Two new baseline-build release artifacts.** The release workflow now produces `cass-windows-amd64-baseline.zip` and `cass-linux-amd64-baseline.tar.gz` alongside the regular artifacts. Both are built with `--no-default-features --features qr,encryption` and have an end-to-end smoke test in CI that asserts `cass --version` runs cleanly on the GitHub-hosted runner. They ship with the same `.sig`/`.crt`/`.sha256` sidecars as every other artifact. Hard-float / SSE2-baseline amd64 hardware (Sandy Bridge, Ivy Bridge, pre-Excavator AMD) can run these binaries; everything except `cass search --mode semantic`, `cass index --backfill quality`, and the embedding-tier maintenance paths continues to work.
- **install.sh / install.ps1 runtime AVX2 detection ([`fb75daab`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fb75daab)).** Both installers now probe the host for AVX2 before choosing an asset. On Linux they read `/proc/cpuinfo`; on Windows-under-MSYS they prefer real CPU flags, then fall back to `wmic cpu get name` / `Get-CimInstance Win32_Processor` model-name heuristics and `Avx2.IsSupported` via PowerShell .NET intrinsics; on ARM/macOS they keep the canonical artifact name. When AVX2 is not detected, the installers pull the `-baseline` artifact automatically. `CASS_FORCE_BASELINE=1` forces the baseline selection on AVX2-capable hosts (useful for testing and for operators who do not need embeddings). A startup AVX2 self-check inside the binary itself remains gated to `semantic` builds so the baseline binary does not abort on pre-AVX2 hosts. JSON goldens were refreshed to reflect the new asset-list shape.

The `RUSTFLAGS=-C target-cpu=x86-64-v2` pin on the canonical Windows build stays as defense-in-depth for the Rust-codegen layer.

### `IndexRunLockGuard` atomic progress bump + ms-precision (cass#258 follow-on)

v0.6.4 introduced the separate `last_progress_at_ms` lock-file field that distinguishes "the heartbeat is alive" from "the indexing thread is making forward progress". Field reports on long single-mode indexing runs surfaced a remaining false-positive: when the indexer was busy with a single multi-batch phase that does not trigger `write_metadata`/`set_mode` calls, `last_progress_at_ms` could stay frozen long enough for `cass health --robot` to flip to `status: "stalled"` even though the indexer was healthy and making batch-level progress.

- **`IndexRunLockGuard::last_progress_at_ms_atomic: Arc<AtomicI64>` ([`397d0443`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/397d0443)).** A lock-free atomic now carries the canonical progress timestamp. The indexer calls a cheap `bump_progress()` after every batch (typed-source replay, embed batch, staging write, checkpoint save, publish), and the background `IndexRunLockHeartbeat` thread folds the in-memory atomic into the on-disk `last_progress_at_ms=` field on every refresh tick. The lock-file fold preserves the v0.6.4 invariant — only the indexer can advance `last_progress_at_ms`, the heartbeat just persists what the indexer already wrote in memory — but it eliminates the per-batch lock-file write cost. The result is that `cass status --json` now reports `last_progress_age_ms` on the order of single-digit milliseconds during normal indexing, instead of seconds-old timestamps that only refresh on phase boundaries.
- **`now_ms` ms-precision plumbing through `InspectSearchAssetsInput`.** The maintenance coordination layer was downgrading the new ms-resolution timestamps to seconds before evaluating the stall threshold, which discarded most of the precision the atomic bump bought us. `InspectSearchAssetsInput` and `evaluate_maintenance_coordination` now thread `now_ms: i64` end-to-end, and the stall threshold comparison is fully ms-precision. Single-mode indexing runs on archives that exceed the v0.6.4 stall threshold no longer false-positive `stalled` in `cass health` / `cass status --json` / the search-side single-flight coordinator.

### Other

- The `## Unreleased` placeholder note added during v0.6.4 has been folded into this entry; the v0.6.3 entry's correction block remains in place as the historical record of the partial-fix → complete-fix transition for cass#256.

## [v0.6.4] -- 2026-05-27

**Critical fix: upstream frankensqlite BtCursor infinite-loop on multi-level B-trees (cass#259), bundled with the v0.6.3-era cass#258 stalled-status liveness work and the cass#257 quality semantic backfill telemetry that landed during the same window.**

### BtCursor forward-progress on multi-level B-trees (cass#259)

Bumped the `frankensqlite` / `fsqlite-types` pin from the published [`0.1.4`](https://crates.io/crates/fsqlite/0.1.4) release up to the newly published [`0.1.5`](https://crates.io/crates/fsqlite/0.1.5) crates.io release, switching back to the registry source (no more git rev pin in `Cargo.toml`; the `[patch.crates-io]` bridge that v0.6.3 carried as a temporary forward-progress hold for `fsqlite-types` is gone). `fsqlite` 0.1.5 contains the upstream [frankensqlite#95 BtCursor forward-progress fix](https://github.com/Dicklesworthstone/frankensqlite/issues/95): a `BtCursor` traversing a B-tree whose root page split into an interior + multiple leaf pages could re-enter the same leaf indefinitely because the cursor stack popped past the interior parent without advancing the parent's child-index cursor before re-descending. The visible symptom on cass was a `cass index` / `cass status --json` / `cass search` process pinning a single core at 100% with zero forward progress on any non-trivial archive (~100 MB+ SQLite DB, or ~50k+ conversations indexed) — the exact wedge profile @blueraz0r reported in #258 and that the v0.6.3 fsqlite 0.1.4 bump did *not* fully close. Reproduces deterministically on a v13 schema once the canonical conversations B-tree grows past its single-leaf-root size; reproducer notes are on the upstream issue. Operators who hit the v0.6.3 wedge should retest under v0.6.4 with no DB surgery required — the cursor fix is purely in the read path and does not require reindexing.

### Forward-progress liveness for wedged rebuilds (cass#258)

@blueraz0r's v0.6.2 [#258 watcher report](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/258) exposed a structural gap in cass's liveness signaling: a wedged indexer (one CPU-bound thread, all other workers parked) was reported as `status: "rebuilding", rebuild.active: true` for 4+ hours because the background `IndexRunLockHeartbeat` thread kept refreshing `index-run.lock`'s `updated_at_ms` field independently of whether the indexing thread was actually making progress. `cass health`, `cass status`, and the maintenance coordination layer all consumed `updated_at_ms` as their liveness signal and were therefore fooled. v0.6.3's fsqlite 0.1.4 bump is the strong candidate to fix the underlying spin (same upstream root cause as #254/#255 — see the v0.6.3 release notes), but the structural liveness gap is fixed here regardless so the next indexer wedge of this shape will no longer be silent.

- **`IndexRunLockGuard` now writes a separate `last_progress_at_ms` field that ONLY the indexing thread updates** — on every `write_metadata`/`set_mode` call (mode/phase transitions the indexer itself initiates are, by definition, forward progress). The background heartbeat refreshes `updated_at_ms=` only and preserves `last_progress_at_ms=` verbatim. A new regression test (`heartbeat_preserves_last_progress_at_ms_field_for_stall_detection`) pins this invariant against future drift.
- **`cass health --robot` and `cass status --json` now report `status: "stalled"`** (distinct from `"rebuilding"`) when the rebuild is nominally active but the indexing thread has been silent for longer than `CASS_REBUILD_STALL_DETECT_SECS` (default 120s, set to 0 to disable). The structured rebuild block also exposes `stalled: bool`, `last_progress_at` (RFC3339), and `last_progress_age_ms`. The `evaluate_maintenance_coordination` layer also degrades stalled snapshots to `Stale` so search-side single-flight callers route around wedged workers instead of attaching to them.
- **`IndexStallWatchdog` no longer short-circuits on `phase_code == 0`.** The previous `if phase_code == 0 || ...` gate at the top of `IndexStallWatchdog::observe` was exactly why #258's watcher emitted zero `stall_detected` events during the 4 h wedge — `phase` never advanced past the Preparing pseudo-phase, so the watchdog stayed silent. The repeat guard `stall_reported_for_phase == Some(phase_code)` still prevents log spam from a single stalled phase. Regression test: `watchdog_fires_on_phase_zero_startup_wedge`.

Tests: `tests/.../search::asset_state::tests::lexical_state_reports_stalled_when_progress_is_stale_despite_fresh_heartbeat`, `..._stays_building_when_progress_is_recent`, `..._does_not_stall_when_legacy_lock_omits_progress_field`, `..._coordination_reports_stale_when_forward_progress_is_stuck`; `indexer::tests::heartbeat_preserves_last_progress_at_ms_field_for_stall_detection`; `stall_diagnostics_tests::watchdog_fires_on_phase_zero_startup_wedge`.

### Quality semantic backfill hardening (cass#257)

Three sub-fixes from @DanielsLoud's comprehensive [cass#257](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/257) proposal landed independently so they can be reverted in isolation if needed. The SQL-shape perf optimizations and batch-watchdog env vars from the same proposal are deferred to a follow-up issue until we have telemetry-driven thresholds.

- **Progress JSONL sink for quality semantic backfill telemetry.** Setting `CASS_SEMANTIC_PROGRESS_JSONL=/abs/path/to/progress.jsonl` appends one JSON object per transition event during semantic backfill. 16 named events (`selection_{start,done}`, `packet_replay_{start,progress,done}`, `embed_batch_{start,done}`, `staging_write_{start,done}`, `checkpoint_save_{start,done}`, `publish_{start,done}`, `error`, `cancelled`, `complete`) carry a wall-clock timestamp, phase + sub-phase classification, batch/row counters, byte counts (so a stalled query is distinguishable from a stalled model), wall-time delta since the sink opened, and a cheap RSS estimate. The sink is silent when the env var is unset, so it has zero cost on the normal operator path. Best-effort writes — a failed write logs at debug and never crashes a backfill that would otherwise succeed.
- **Per-message `last_message_id` checkpoint cursor with durable resume.** The semantic checkpoint manifest now persists the highest canonical message PK embedded in the most recent batch, in addition to the existing conversation offset. Resume strictly filters out messages with `id <= last_message_id`, so an interrupted bounded backfill never re-embeds messages already staged. The manifest format version bumped 1→2; pre-#257 binaries reading a v2 manifest get a clean `UnsupportedVersion` error, and post-#257 binaries reading a v1 manifest fall back to the conversation offset gracefully with a one-shot warning that resume granularity is coarser than ideal until the next checkpoint save.
- **`cass status` quality-tier-aware reporting.** The status JSON now carries two additive fields: `semantic.quality_tier_published` (true when the quality vector index is published and matches the current DB fingerprint, independent of the fast/progressive stack) and `semantic.semantic_only_search_available` (true when at least one tier is queryable). Operators querying with `--mode semantic` against a quality-only published index no longer see the surface incorrectly reporting "building/unavailable" just because the progressive/hybrid stack hasn't been backfilled.

Tests: `tests/cass_257_semantic_progress_jsonl.rs` (sub-fix 1, end-to-end against a fixture corpus), `tests/cass_257_checkpoint_last_message_id.rs` (sub-fix 2, write-kill-restart resume + forward-compat fallback), `tests/cass_257_status_quality_tier_aware.rs` (sub-fix 3, JSON-shape assertions against a quality-only fixture).

## [v0.6.3] -- 2026-05-27

**Critical fixes: v0.6.2 startup panic / indexing-and-query stall (upstream frankensqlite regression) and Windows binary illegal-instruction on pre-AVX2 CPUs.**

- **`cass` no longer panics on startup with "range end index 27 out of range for slice of length 25" (#254).** Bumped the `frankensqlite` / `fsqlite-types` pin from the buggy git rev `b3c841b`/`68426d3e` (which was carried inside v0.6.2 to ship the #252 witness cap) up to the published [`0.1.4`](https://crates.io/crates/fsqlite) crates.io release. `fsqlite` 0.1.4 fixes the upstream [#93 `execute_join_select` panic](https://github.com/Dicklesworthstone/frankensqlite/issues/93): a virtual-table cursor over a v13 schema with an FTS5 vtable was over-counting the row width by the hidden rowid column, producing an off-by-two slice-end index in the join driver. The panic fired on `cass --verbose`, `cass status --json`, `cass search`, `cass stats`, and the `cass index --watch` startup phase for any DB that had an FTS5 virtual table on it (every v0.6.1+ install). The dependency bump landed in commit [`2566b32f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2566b32f); this release tags it and ships rebuilt binaries.
- **Full / incremental / watch rebuilds and `cass search`/`cass stats` no longer spin at ~99% CPU forever on 0/N conversations (#255).** Same root cause as #254 — the panic-then-restart loop inside the upstream frankensqlite query executor presented to operators as an apparent indexing stall: high CPU, growing RSS, zero progress on the `current_conversations` counter, `page_prep_workers: 0`, `controller_mode: pinned_steady`, and `lsof` showing only the SQLite DB plus its lock file (no Tantivy shard FDs). The witness cap (#252 fix) prevented the OOM symptom but the query plan still trapped inside the buggy `execute_join_select` path. The fsqlite 0.1.4 bump unblocks query execution end-to-end. Reproduced on Linux x86_64 (#254 reporter, #255 reporter) and macOS arm64 (#255 reporter follow-up). The 0.1.4 release also carries the FTS5 delete-all+reinsert PrimaryKeyViolation fix ([fsqlite #94](https://github.com/Dicklesworthstone/frankensqlite/issues/94)), which was the secondary failure mode that surfaced once the slice panic was bypassed via DB surgery in the field.
- **Windows binary on pre-AVX2 CPUs (#256) — PARTIAL FIX ONLY; see correction below.** The v0.6.2 `cass-windows-amd64.zip` artifact illegal-instructioned (`STATUS_ILLEGAL_INSTRUCTION` / `0xC000001D`) at process start on Sandy/Ivy Bridge hardware (e.g. Intel Core i7-3770K, 2012). The release workflow now pins the Windows build to `RUSTFLAGS=-C target-cpu=x86-64-v2` — the SSE4.2 + POPCNT microarchitecture level that matches every 64-bit Windows host shipped since ~2009. Linux and macOS jobs were already on the conservative default `x86-64`/`apple-arm64` baseline and do not need changes.

> **⚠️ Correction (2026-05-28):** The `RUSTFLAGS=-C target-cpu=x86-64-v2` constraint above is **necessary but not sufficient** for #256. Reporter @Dlows-Vibe empirically confirmed that v0.6.3 (and v0.6.4) still crash on Ivy Bridge with the same `0xC000001D` exit. Root cause: the `fastembed` feature in `Cargo.toml` enables `ort-download-binaries-rustls-tls`, which downloads **prebuilt Microsoft ONNX Runtime binaries** at build time. Those prebuilts ship with AVX/AVX2/FMA-dispatched code already compiled in — `RUSTFLAGS` only constrains `rustc`'s own codegen and cannot reach object code linked from a vendor prebuilt. The crash fires in static init before any user code runs, which is why even `cass --version` dies.
>
> **Status:** #256 has been reopened. The complete fix requires feature-gating the `fastembed`/`ort` stack so that a separate `cass-windows-amd64-baseline.zip` artifact (no embeddings, pure CPU baseline) can ship for pre-AVX2 hardware. Tracking in v0.6.5. The RUSTFLAGS constraint stays in the workflow as defense-in-depth for the Rust-codegen layer.

## [v0.6.2] -- 2026-05-24

**Critical regression fixes: multi-GB allocation on every SQL query (#252) and silent-exit on `cass mirror prune` (#253).**

- **`cass mirror prune` is no longer a silent no-op (#253).** The CLI dispatcher routed `Commands::Mirror(..)` into the wrong outer branch in `execute_cli` (`src/lib.rs`). The outer arm pattern listed `Mirror(..)` alongside `Index | Search | Pack | ...`, but the inner `match command { ... }` inside that branch had no `Commands::Mirror(..)` arm; the actual dispatch lived in the sibling `_ =>` branch and was therefore unreachable for every mirror invocation. The user-visible symptom was `cass mirror prune` (and every flag combination, including `--dry-run`, `--apply`, `--json`) exiting 0 with empty stdout and empty stderr. Same root cause for `cass import` — `Import(..)` was also wrongly listed in the early arm. Removing both from the early arm pattern lets them fall through to the `_ =>` branch where the dispatch already exists. A new `tests/cli_mirror_prune.rs` integration suite pins the contract that the success path emits a plan/summary and the no-args path errors out with the documented usage message.
- **Multi-GB RSS allocation on every SQL-touching query is gone (#252).** Bumped `frankensqlite` from `c8ce64fd` to [`b3c841ba`](https://github.com/Dicklesworthstone/frankensqlite/commit/b3c841ba), which adds an opt-in `FSQLITE_READ_WITNESS_CAP` env-var cap on the per-cursor `read_witnesses: Vec<WitnessKey>`. On the v0.5.1-bisected `cass stats --json` case, a `SELECT COUNT(*)` over a 3.3 GB index allocated ~5.5 GB RSS because the cursor's witness vec grew one entry per page touched during the B-tree descent (~42-49k `btree_descent` prefetch hints in 5 s, no responsiveness backpressure). cass's `main.rs` now sets `FSQLITE_READ_WITNESS_CAP=16384` by default at process startup (via `std::env::var_os` so any user override wins). cass is a read-mostly analytical workload that does not consume the per-cursor witness cache — the canonical SSI provenance still flows into the pager regardless of this cap, so the cap is safe and does not weaken isolation. Operators who need the historical unbounded behavior can export `FSQLITE_READ_WITNESS_CAP=0` before launching cass.

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

[Unreleased]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.7.1...main
[v0.8.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.7.1...main
[v0.7.1]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.7.1
[v0.7.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.6.26...1f6cdcf9
[v0.6.25]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.6.24...v0.6.25
[v0.6.24]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.6.23...v0.6.24
[v0.2.2]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.1...v0.2.2
[v0.2.1]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.0...v0.2.1
[v0.2.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.64...v0.2.0
[v0.1.64]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.50...v0.1.64
[v0.1.50]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.36...v0.1.50
[v0.1.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.0
