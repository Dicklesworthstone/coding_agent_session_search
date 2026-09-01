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

## 7. Bridge plan

Ordering principle: stop user-visible bleeding and restore the gate first,
then make the docs true, then pay down structure. Every workstream lists the
concrete change, acceptance, and the test that proves it.

### WS-A — Restore the quality gate (first 48 hours)
A1. Re-enable `CI` and `Fresh Clone Build` workflows (or a self-hosted/rch-backed equivalent) with: fmt, clippy `-D warnings`, `cargo check --all-targets`, `cargo test --lib`, UBS on changed files. Acceptance: a green run on main within 24 h; red main blocks bead closure by convention.
A2. Fix the 3 red lib tests (bet45): bisect f273ccc4 (content-bounded page limit) and 7af85c82 (strict read-only page-prep open) before blaming the engine; audit the "error before `wait_for_turn`" path named in cjugu. Acceptance: `cargo test --lib` 100% green, no `#[ignore]` added.
A3. Produce one full integration-suite receipt (`cargo test --all-targets` via rch, batched) and record failures as beads. Acceptance: receipt commit with pass/fail/ignored counts.
A4. Add `#![deny(unsafe_code)]` at crate root with `#[allow(unsafe_code)]` on the 12 audited sites (env at main.rs:185/218; FFI in daemon/resource.rs, indexer/mod.rs renameat2, responsiveness.rs getloadavg, tui_asciicast.rs fcntl, semantic_manifest.rs Windows attrs). Delete the string-grep Send/Sync test. Acceptance: builds; grep test removed.
A5. Golden hygiene: add `triage.json.golden`; delete the 22 untracked `.actual` files (owner permission required per AGENTS.md rule 1); move or symlink swarm goldens under `tests/golden/robot/`. Re-bless the 15 February snapshot baselines with the TESTING.md review checklist.
A6. Re-enable `Benchmarks` with `search_latency_e2e.rs` as a ratchet (p95 engine ≤ 60 ms on the bench corpus; one-shot CLI ≤ 250 ms after WS-B7).

### WS-B — Large-archive reliability (the P0 user class)
B1. **#441**: run Quill compaction at the end of every one-shot `cass index` (not only in the watch closure, mod.rs:16586); expose `CASS_QUILL_QUERY_FUEL_BUDGET` and set the reader's `QuillConfig` from it (quill_bridge.rs ~545); in hybrid, catch lexical fuel exhaustion and degrade to the semantic leg (or lexical-with-stopwords-dropped) with `lexical_fallback_reason:"query_fuel_exhausted"` instead of `?` at query.rs:6907. Tests: append-only corpus × N incrementals → segment count bounded; 8-word stopword query on 600-segment fixture returns hits; robot meta reports the degrade.
B2. **#439**: emit progress ticks from `repair_fallback_fts_after_full_index_run` (mod.rs:9672) and daily-stats rebuild; make the stall watchdog phase-aware so post-publish phase 0 is report-only unless block IO and CPU are both idle for the full window. Test: fixture with published generation + slow FTS repair does not exit 70.
B3. **#440**: in `reconcile_pending_lexical_commit` (mod.rs:8716) compare the durable cursor against the published Quill doc count/manifest and either advance the cursor or restart the staged rebuild; never exit 9 on a self-inflicted cursor lag. Test: SIGTERM mid force-rebuild → next plain `index` exits 0.
B4. **health bounded open**: route `health` through `probe_state_db_strict_bounded` (lib.rs:19846) like `status`; forbid `attempt_dirty_wal_recovery_checkpoint` on any observation surface. Test: 200 MB dirty WAL fixture → `health` returns < 500 ms and WAL size unchanged.
B5. **#391**: add a bead and a detection: run `quick_check` + rowid-monotonicity probe on `conversations` in `doctor check`; capture a support bundle automatically when the engine migration marker reports repaired orphaned pages. Test: corrupted-fixture → doctor reports `btree_rowid_order` finding.
B6. **Memory governor in sqlite.rs**: consult `responsiveness` in FTS rebuild paging and migration V18–V20 (batch by keyset, commit per batch); pre-check free space ≥ 2× DB before any engine migration and surface the engine backup in doctor's asset taxonomy with a reclaim path. Tests: 1M-row fixture rebuild under 1 GB RSS cap; migration refuses when disk < 2× DB.
B7. **Robot per-call overhead**: cache the storage-integrity preflight result keyed by (db mtime, size, WAL size) in `<data_dir>/state/`, skip it on the read path when fresh, and open read-only without recovery. Target: `other_ms` < 150 on this host. Test: bench asserts one-shot `search --robot` ≤ 250 ms warm on the 10 GB fixture.
B8. **#395 residual**: defer `load_semantic_context` off the first-frame path (use the `_deferred` variant, model_manager.rs:600); never run `rebuild_analytics` in-process from the TUI — spawn `cass analytics rebuild --days 2` detached and show a toast. Test: headless TUI on the 2M-message fixture renders first frame < 2 s.
B9. **GH#426 ledger** (fyepq): implement the per-source observation ledger so interrupted incremental runs resume from the last committed source; retire or wire `refresh_ledger.rs`. Test: kill mid-incremental → re-run reparses only unfinished sources.
B10. Keep #413/#422 acceptance beads open until reporter-sized scratch proofs are attached.

### WS-C — Semantic: ship it or stop selling it (decision required)
C1. Decision gate: either (a) fund ds7uy.1–.5 + wfm4e + jyfuq as one owned program with a frankensearch counterpart, or (b) cut README to "hybrid = lexical + single-tier MiniLM refinement; progressive and ANN are experimental flags." Default recommendation: **(b) now, (a) as a scheduled program**, because today's README promises a capability no test has ever exercised.
C2. Real-model test lane: add a CI job (opt-in, cached) that downloads the safetensors MiniLM once and runs the 8 ignored embedder/reranker tests plus a restart/degrade/recover e2e (jyfuq). Replace the int8 ONNX fixtures.
C3. Make `hnsw_ready` reflect native admission (wfm4e); build the ANN sidecar in backfill when the quality tier completes; let hybrid use ANN without `--approximate` when admitted.
C4. Manifest: either land the writer (ds7uy.1) together with the reader (ds7uy.3) in one change, or delete the writer-less scaffolding and the `current.json` fail-closed reader. Never land one half.
C5. Collapse the four readiness enums to one state machine feeding health/status/query (ds7uy.4).
C6. Fix contract leaks: `preferred_backend:"fastembed"` → `"native"`, remove `OnnxEmbedderConfig`, README 256-d → 384-d.

### WS-D — Make the docs true
D1. README correction pass driven by the §2 table: schema v20; dedup keys; keyboard table regenerated from `impl From<Event> for CassMsg`; density rows; analytics/bulk keys; remove bookmarks or wire them (WS-E3); `--timeout` semantics; swarm = fixture-only until WS-F3; rsync flags; HTML export flags/JSON/exit 9; Tailwind statement; api-version fields; Homebrew tarballs; glibc gate reality; self-update reality; sub-60 ms claim scoped to engine time.
D2. Document the hidden surface: `cass pages` (with a security note and the real key-recovery flow), `upgrade`, `man`, `storage`, `dedup`, `support-bundle`, `state`, `onboarding`, `quarantine`, `forget`, `fleet`, `lessons`, `import`, `release-verify`, `sources discover|reingest|artifact-manifest`.
D3. AGENTS.md: connector table (26 incl. goose, muse), CI section rewritten to state that gates are agent-run via rch until WS-A1 lands, schema note, `RCH_REQUIRE_REMOTE` behavior.
D4. In-app help and `mistake_recoveries` text aligned with code (Alt+N/Alt+I; typo correction statement).
D5. Emit teaching notes on stderr in robot mode too (lib.rs:6730) — agents are the audience the README names.
D6. Add a `docs/validate_docs.sh` check that every README key binding and CLI flag exists in code (keys via the `CassMsg` map; flags via `cass introspect --json`). Make it part of WS-A1.

### WS-E — Dead code and structure
E1. Delete the Tantivy-only staged-shard pipeline (`rebuild_tantivy_from_db_via_staged_shards` and the ~3.7K lines listed in the lexical audit), the 674-line federated helpers in tantivy.rs, the ~10 no-op `CASS_TANTIVY_*` vars, and the 31 "Tantivy-only" ignored tests. Keep `lexical_tantivy` only for the differential oracle. (File deletions need owner permission per AGENTS.md rule 1; code removal within files does not.)
E2. Rename the Tantivy vocabulary (`rebuild_tantivy_from_db*`, user-facing "Tantivy lexical index completed") to Quill; keep env-var aliases for one release with a deprecation note.
E3. Bookmarks: wire `bookmarks.rs` to a TUI key + palette action + `cass bookmarks list|add|export` CLI, or delete the module and the README section. Recommendation: wire it — the design is complete and advertised.
E4. Remove `two_tier_search.rs` (or make it the progressive implementation under WS-C), `ProbeCache`, and the `backtrace` feature.
E5. lib.rs decomposition (mechanical, golden-guarded, no behavior change): move `run_doctor_impl` + doctor helpers to `src/doctor/`, search rendering (`output_robot_results`, `run_cli_search`) to `src/search/cli/`, `run_index_with_data` to `src/indexer/cli.rs`, export-html to `src/html_export/cli.rs`, pack to `src/search/pack_cli.rs`, status/health to `src/status/`, schemas to `src/introspect.rs`. Target: lib.rs < 30K lines; no function > 300 lines in moved code. Same treatment for `indexer/mod.rs` (split pipeline/publish/watch) and `ui/app.rs` (split update arms by surface).
E6. Make `doctor.rs:1543`/`lib.rs:71782` stop string-scanning source text; replace with a compile-time registry test.

### WS-F — Robot contract fixes
F1. `--timeout`: return partial hits with `_meta.timed_out:true` and exit 8 (as documented), or change the docs. Recommendation: implement exit 8; agents already branch on it.
F2. Exit 70 must go through the error envelope (`kind:"index-stalled"`).
F3. Swarm: implement live adapters (beads: read `.beads/issues.jsonl`; Agent Mail: read the local archive or HTTP `/mcp`; rch: `rch status --json`; git: existing) behind the existing adapter trait, or label the commands `--fixture`-only in README and capabilities.
F4. Add `_meta.cache_hit`, error on unknown `--source`, `<mark>` in HTML export highlight, fix api-version sample.
F5. Unify the 4 snake_case error kinds to kebab-case with a one-release alias.

### WS-G — Peripheral correctness
G1. `sources setup`: actually run the final sync unless `--skip-sync`; make `install()` fall through the documented chain.
G2. Installer: implement the glibc ≥ 2.38 probe with `--from-source` fallback; self-update: back up the current binary and restore on failed verify.
G3. HTML export: add `--password` (with a warning) or remove it from docs; load Tailwind as documented or drop the claim; align the robot JSON shape with README or vice versa; document exit 9.
G4. Pages: implement `cass pages key list|add|revoke|rotate` on top of `key_management.rs`, or rewrite docs/RECOVERY.md to the real recovery flow; fix `lighthouse.yml` bogus flag.
G5. Doctor asset taxonomy: classify `*.pre-migration-bak*` and `.fsqlite-migration-state`, report their size, and offer a fingerprinted cleanup plan.
G6. Un-ignore the SSH e2e tests in a Docker-capable CI lane.

### WS-H — Tracker hygiene (do immediately, cheap)
H1. Close done-but-open beads with the landing commit in the closing note: rhmbf, lukne, 34irx, zqre2, and the 12 Pages/secret-scan beads (verify each against code first).
H2. Close obsolete crates.io beads hvzel, wssow, 1ixp7 (0.7.0 published 2026-08-25).
H3. File beads for every §5.1 gap; file `gh439-*`, `gh440-*`, `gh441-*`, `gh395-*`, `gh391-*`, `gh423-*` per convention.
H4. Re-triage the 21 stale in_progress beads: either re-own with a dated note or return to open.
H5. Correct bet45's root-cause narrative (cass-side suspects first).
H6. Run `br doctor --repair` on a preserved copy (owner's call), fix the stale `beads.base.jsonl` merge anchor, remove the duplicate `br` from PATH.
H7. Make "GitHub issue filed ⇒ bead within 24 h" a swarm rule in AGENTS.md.

### Suggested sequencing
Week 1: WS-H (day 1), WS-A1–A3, WS-B1–B4, WS-D5.
Week 2: WS-B5–B8, WS-A4–A6, WS-F1–F2, WS-C1 decision + C2 lane.
Week 3: WS-D1–D4, WS-G1–G3, WS-E1–E4.
Week 4+: WS-E5 decomposition (rolling, golden-guarded), WS-C3–C6 if (a) chosen, WS-F3, WS-G4–G6, WS-B9.

---

## 8. Answers to the five reality-check questions

1. **What IS working:** the whole lexical core loop, robot ergonomics, doctor v2, scheduler, raw mirror, redaction, daemon attestation, packaging, Pages (hidden), and the storage safety story (no unsafe Send/Sync, rusqlite gone).
2. **What is NOT:** progressive/two-tier semantic search (does not exist at runtime), ANN readiness, bookmarks, swarm live composition, `--timeout` partials, bounded `health`, setup's final sync, installer glibc gate, self-update rollback, and — most importantly — index/search reliability on multi-GB append-only archives (#439/#440/#441/#395/#391/#379/#345/#329/#349/#320).
3. **What is blocking:** engine-boundary scale behavior, no CI ratchet, the lib.rs/indexer monolith, a swarm process that closes beads faster than it tracks user issues, and an ownerless two-repo semantic program.
4. **Would all open + in-progress beads close the gap?** No — they cover the semantic architecture (if executed), Windows, rustsec, some engine-blocked memory items and Pages hardening, but none of the §5.1 list (today's three issues, corruption detection, CI, docs truth, dead code, structure, bookmarks, timeout contract, per-call overhead).
5. **Vision goals with zero bead coverage:** see §5.1 — 17 items, led by #441/#439/#440, #395 residual, #391, CI re-enablement, README truth, and lib.rs decomposition.
