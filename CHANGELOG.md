# Changelog

All notable changes to **cass** (coding-agent-session-search) are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) with links to representative commits.
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Repository: <https://github.com/Dicklesworthstone/coding_agent_session_search>

> **Releases vs. tags**: Only [v0.1.64](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.64), [v0.2.0](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0), [v0.2.1](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.1), and [v0.2.2](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.2) have published GitHub Releases with downloadable binaries. All other version numbers below are git tags only (no release artifacts).

---

## [Unreleased] (after v0.2.2)

Work in progress on `main` since the v0.2.2 tag.

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
