# Reality Check and Bridge Plan — cass at v0.7.1+55 (2026-09-01)

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
