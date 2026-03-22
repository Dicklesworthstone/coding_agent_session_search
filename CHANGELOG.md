# Changelog

All notable changes to **cass** (coding-agent-session-search) are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) with links to representative commits and releases.
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Repository: <https://github.com/Dicklesworthstone/coding_agent_session_search>

---

## [Unreleased] (after v0.2.2)

Work in progress on `main` since the v0.2.2 tag.

### Added

- **HTML/PDF export pipeline rewrite**: Complete overhaul of the export rendering system with improved layout and PDF support ([`98757e6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/98757e67))
- **Parallel indexing**: Indexer can now process multiple connector sources concurrently ([`40627d2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/40627d25))
- **TUI search overhaul**: Redesigned search interaction in the TUI with improved result rendering ([`40627d2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/40627d25))
- **FTS5 contentless mode (schema V14)**: Migrate full-text search to contentless FTS5 tables, reducing database size while maintaining query performance ([`5a30465`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5a304657))
- **LRU embedding cache**: Progressive search caches embeddings via LRU to avoid redundant ONNX inference ([`a8f7a52`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a8f7a522))
- **Analytics dashboard expansion**: Additional chart types, structured error tracking for analytics queries, and improved app layout ([`b393593`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b3935935), [`f073b99`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f073b994))
- **Click-to-position cursor**: Click anywhere in the search bar to position the cursor, with pane-aware hover tracking ([`69d2518`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/69d25182))
- **UltraWide breakpoint**: New layout breakpoint for ultra-wide terminals with style system refactoring ([`baf3310`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/baf33104))
- **Sparkline bar chart in empty-state dashboard**: Visual indicator in the dashboard when no data is loaded ([`3fb1c44`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3fb1c447))
- **Footer HUD lanes**: Conditional footer HUD with compact formatting and refined empty-state display ([`bf314fb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bf314fba))
- **WAL corruption detection**: Degraded health state reported when WAL corruption is detected ([`a738a9b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a738a9b0))
- **Pages subsystem expansion**: Config input, encryption, and export improvements for the static-site export system ([`426d6fe`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/426d6fe5))

### Changed

- **Complete rusqlite-to-frankensqlite migration**: All production and test code now uses frankensqlite exclusively; rusqlite fully extirpated ([`e372307`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e3723076), [`232bdd1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/232bdd16))
- **Remove reqwest dependency**: HTTP operations migrated to asupersync HTTP client; Cloudflare deploy and model download use the new client ([`dc90e9f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dc90e9f7), [`80d9885`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/80d98854))
- **Search query pipeline restructuring**: Improved phase coordination and progressive search integration ([`03442ce`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/03442ce7))
- **Search-as-you-type supersession**: In-flight searches are cancelled when a new keystroke arrives ([`e163926`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e163926c))

### Fixed

- **Export skill content leak**: Proprietary skill messages are now stripped from HTML, Markdown, text, and JSON exports ([`dd568dc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dd568dc8), [`e1886a0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e1886a0e))
- **UTF-8 panic in smart_truncate**: Fixed panic on multi-byte character boundaries ([`c874303`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c8743037))
- **XSS prevention**: Defensive string slicing and HTML sanitization in simple HTML export ([`4fcc026`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4fcc026e))
- **Display width correctness**: Label measurement and truncation now use Unicode display width instead of char count ([`76d8671`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/76d86714))
- **Backup cleanup**: Skip directories and exclude WAL/SHM sidecars from backup cleanup rotation ([`a5c9e75`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a5c9e756), [`2ad0bf6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2ad0bf66))
- **Windows-safe atomic file replacement**: Config and sync state files use platform-safe replacement ([`9353938`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/93539383))
- **Two-tier blended scoring**: Penalize unrefined documents in blended scoring to prevent stale results from dominating ([`b0c612c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b0c612cd))
- **NaN-safe score normalization**: Prevent NaN from propagating through score normalization pipeline ([`1eb68aa`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1eb68aa9))
- **Update cadence persistence**: Defer cadence file write until after successful fetch, preventing stale cadence on network failure ([`f6cebc8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f6cebc85))
- **Zero compiler warnings**: Eliminated all remaining compiler warnings across the codebase ([`3c83c68`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3c83c680))

---

## [v0.2.2] - 2026-03-15 (GitHub Release)

[Release page](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.2) | [Compare v0.2.1...v0.2.2](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.1...v0.2.2)

### Fixed

- **FrankenSQLite FTS5 registration**: Register FTS5 virtual table on frankensqlite search connections; fixes search failures after the v0.2.0 migration ([`f3acfec`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f3acfecb), [#26](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/26))
- **Secret redaction before DB insert**: Redact secrets from tool-result content before storing in the database ([`eb9444d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/eb9444d0), [#112](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/112))
- **Doctor FTS rebuild OOM**: Chunk FTS rebuild in doctor command to prevent out-of-memory on large databases ([`3e736ab`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3e736ab4))
- **Doctor FTS rebuild SQL**: Correct SQL syntax and add transaction safety with ROLLBACK on failure ([`75e2008`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/75e20085), [`afad4e9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/afad4e9a))
- **Replace sqlite_master queries**: Use direct table probes instead of `sqlite_master` queries for FrankenSQLite compatibility ([`892d1bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/892d1bd0))
- **Unwrap elimination**: Replace unwrap calls with safe error handling across search, export, timeline, and tests ([`300caa4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/300caa4b), [`900abdf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/900abdfa))
- **Null-safety in JS**: Add null-safety guards in router, service worker, and perf tests for Pages export ([`c5f64c3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c5f64c35))
- **Colorblind theme redesign**: Redesigned colorblind palette and fixed preset cycling bugs ([`6807be3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6807be3f))
- **Daemon concurrency**: Eliminate connection cloning; handle requests concurrently in the daemon ([`87e8b3d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/87e8b3df))
- **Indexer stale detection**: Correct stale detection grace period and redact JSON keys in logs ([`cf5fc17`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cf5fc17c))
- **Pages hardening**: Harden decrypt, preview server, and clean up exclusion API ([`827ece2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/827ece29))
- **Missing-subcommand hints**: Display helpful suggestions when a user types an invalid subcommand ([`c0cf17a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c0cf17a3))

### Changed

- **Export rendering from DB**: Load sessions directly from the SQLite database instead of JSONL files, improving export speed and reliability ([`3338ac3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3338ac38))
- **Analytics parameter builders**: Use `ParamValue` directly in query parameter builders for cleaner code ([`c8d22d3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c8d22d3c))
- **glibc 2.38+ requirement documented**: Pre-built binaries now document the minimum glibc version ([`c8883aa`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c8883aad))

---

## [v0.2.1] - 2026-03-09 (GitHub Release)

[Release page](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.1) | [Compare v0.2.0...v0.2.1](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.0...v0.2.1)

### Added

- **Colorblind accessibility theme**: New preset for deuteranopia/protanopia with tested color collisions ([`0133256`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/01332563))
- **Kimi Code and Qwen Code connector stubs**: Re-export stubs for Kimi Code and Qwen Code agents ([`886af59`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/886af59e))
- **Copilot CLI connector**: New connector module for GitHub Copilot CLI sessions ([`e87d6f1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e87d6f18))
- **Incremental embedding in watch mode**: Semantic embeddings update incrementally during watch-mode indexing ([`d746f99`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d746f993))

### Fixed

- **Static OpenSSL linking**: Statically link OpenSSL to eliminate `libssl.so.3` runtime dependency on Linux ([`efe5d32`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/efe5d321), [#109](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/109))
- **Lower glibc floor**: ARM64 glibc floor lowered to 2.35 for broader Linux compatibility ([`074a678`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/074a6781))
- **TUI resize logging disk exhaustion**: Make resize evidence logging opt-in to prevent disk fills ([`c343ac9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c343ac92), [#108](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/108))
- **Export modal key consumption**: Enter and navigation keys now properly consumed in export modal ([`fc2b3d6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/fc2b3d67), [#96](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/96))
- **Scoop manifest and PowerShell installer**: Fix Scoop manifest URL and PowerShell checksum verification on Windows ([`7bd3a02`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7bd3a028), [#93](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/93))
- **Windows installer temp path**: Use `.DirectoryName` for provider-neutral temp path on Windows ([`d4b5b5e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d4b5b5eb))
- **Tool role messages in export**: Include "tool" role messages in all export formats ([`e32ee69`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e32ee693))
- **Health --json real DB stats**: `cass health --json` now reports real database statistics ([`6ce238b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6ce238b9))
- **Pane scroll width**: Use full results-strip width for pane scroll capacity calculations ([`9996a7f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9996a7f1))
- **CI runners**: Use `ubuntu-24.04` runners for Linux release builds ([`050db98`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/050db985))
- **FrankenSQLite pin churn**: Resolved through 6 pin iterations to align btree, mvcc, pager, vdbe, and lifecycle crates ([`37085910`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/37085910)...[`a566edd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a566edd2))

### Changed

- **FrankenSQLite migration tests**: Comprehensive migration tests and benchmarks with `BEGIN CONCURRENT` support ([`cd4f3bb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cd4f3bb2))
- **Frankensearch imports**: Import `CassFields` and `CASS_SCHEMA_HASH` directly from the frankensearch crate ([`c1eb61e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c1eb61eb))
- **Hardened release pipeline**: Checksum verification and updated Homebrew formula for v0.2.0 ([`9518643`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9518643e))

---

## [v0.2.0] - 2026-03-02 (GitHub Release)

[Release page](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0) | [Compare v0.1.64...v0.2.0](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.64...v0.2.0)

This is a major release representing the migration to a fully custom infrastructure stack: FrankenTUI replaces Ratatui, FrankenSQLite replaces rusqlite, and FrankenSearch replaces the bespoke Tantivy integration.

### Added

#### FrankenTUI (ftui) Migration
- **Complete TUI rewrite**: Migrated from Ratatui to FrankenTUI, a custom immediate-mode terminal UI framework with differential rendering, spring animations, and adaptive degradation ([`81f2560`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/81f25604))
- **CassApp model**: Elm-architecture model with 60+ message variants, 15 context scopes, 100+ keybindings, and FocusGraph-based keyboard navigation
- **Responsive 3-pane layout**: LayoutBreakpoint-driven adaptive splits (Narrow/Medium/Wide) with DensityMode row heights (compact/cozy/spacious)
- **Virtualized results list**: O(visible) rendering supporting 100K+ results via ftui VirtualizedList with Fenwick-tree height prediction
- **Command palette**: Ctrl+P overlay with 14 actions and fuzzy search

#### Analytics Dashboard (8 Views)
- **Dashboard**: 2x3 KPI tile wall with per-tile sparklines, delta indicators, and top agents bar chart
- **Explorer**: Metric selector with overlay breakdowns (by agent/workspace/source), group-by (hour/day/week/month), and zoom levels
- **Heatmap**: Activity heatmap visualization
- **Breakdowns**: Tabbed view with 4 dimensions (Agent/Workspace/Source/Model)
- **Tools / Cost / Plans / Coverage**: Specialized analytics views

#### Animation, Macros, and Clipboard
- **Spring physics**: 7 animation targets with natural-feeling spring-based animations (kill switch: `CASS_DISABLE_ANIMATIONS=1`)
- **Input macro recording**: Alt+M toggle with JSONL serialization and `--record-macro`/`--play-macro` CLI flags
- **Inline mode**: `--inline` flag with scrollback-preserving TUI and `--asciicast` recording
- **OSC52 clipboard**: Native terminal clipboard with multiplexer passthrough; `y`/`Ctrl+Y`/`Ctrl+Shift+C` keybindings
- **Undo/redo**: Ctrl+Z / Ctrl+Shift+Z for query, filter, grouping, and saved view changes (depth 100)
- **JSON viewer**: `J` key toggles syntax-highlighted JSON view of raw session data

#### FrankenSQLite Migration
- **Full V13 schema**: FrankenStorage rewrite with transaction support, `fparams!` macro, and compat gates ([`e5789a7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e5789a7f))
- **BEGIN CONCURRENT**: Support for concurrent write transactions via FrankenSQLite ([`51cf9d5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/51cf9d54))
- **Module-by-module migration**: bookmarks, pages/analytics, pages/export, pages/size, secret_scan, summary, wizard, and 7 lib.rs call sites all migrated ([`89c1a0f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/89c1a0fb)...[`39d3bb0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/39d3bb01))

#### FrankenSearch Migration
- **Unified crate facade**: Consolidated frankensearch sub-crate imports ([`a1593f0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a1593f02))
- **HNSW semantic pipeline**: Migrated semantic search to frankensearch HNSW index ([`ddf6e98`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ddf6e98e))
- **Lexical index migration**: Lexical index now uses frankensearch facade ([`169e4ec`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/169e4ecd))
- **Connector detection delegation**: Connector implementations extracted to `franken_agent_detection` crate ([`78740e7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/78740e73))

#### Export & TUI Features
- **Export tab**: HTML/Markdown export keybindings in the TUI (`e` key, `Ctrl+E` quick export) ([`98863d3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/98863d39))
- **Workspace filtering**: Filter search results by workspace with WCAG theme fixes ([`690506f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/690506f0))
- **Real-time indexer progress bar**: Live progress display with help popup scrollbar ([`71d779b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/71d779be))
- **Word-jump navigation**: Ctrl+Left/Right for word-level cursor movement in search bar ([`e37b817`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e37b8176))
- **18 theme presets**: Expanded theme system with removal of custom color overrides ([`9ff7434`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9ff74349))

#### Infrastructure
- **Cursor pagination**: Cursor-based pagination for CLI search output ([`19d7908`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/19d79089))
- **openrsync detection**: Detect macOS 15+ `openrsync` and skip unsupported `--protect-args` flag ([`76bb7f6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/76bb7f6d))
- **Modern ONNX model layout**: Support `onnx/` subdirectory model layout with legacy fallback ([`99f4385`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/99f43855))
- **MIT + OpenAI/Anthropic rider license**: Adopted new license metadata ([`ec5fbe5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ec5fbe56))

### Changed

- **Adaptive rendering**: FrameBudgetConfig with 16ms/60fps PID degradation and graceful DegradationLevel stepping
- **Differential rendering**: Only changed cells written to terminal, dramatically reducing I/O
- **Theme persistence**: JSON at `~/.config/cass/theme.json` with versioned schema and 19 semantic color slots
- **80x24 compatibility**: Nothing breaks at minimum terminal size

### Removed

- **Ratatui dependency**: Completely removed from Cargo.toml and all source code
- **Legacy TUI module**: `tui.rs` reduced to 4-line stub; all rendering in ftui-based `app.rs`
- **Token count chip**: Removed token count from conversation metadata header ([`0aeabc0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/0aeabc03))
- **Cost analytics view**: Removed from analytics dashboard ([`6ec44e9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6ec44e90))

### Testing

- 50 UI snapshot tests with deterministic frame capture
- 15 macro tests (recording lifecycle, path redaction, JSONL roundtrip, playback injection)
- 5 performance E2E tests (render timing, scaling, optimization chain)
- PTY E2E tests with output-growth assertions
- CI failure artifact forensic bundles

---

## [v0.1.64] - 2026-02-01 (GitHub Release)

[Release page](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.64) | [Compare v0.1.63...v0.1.64](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.63...v0.1.64)

### Added

- **ClawdBot connector**: Full support for ClawdBot sessions (`~/.clawdbot/sessions/`) ([`4744ff5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4744ff51))
- **Vibe connector**: Support for Vibe (Mistral) agent logs ([`38d44bb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/38d44bb9))
- **ChatGPT web export import**: `cass import chatgpt` command with auto-detection and idempotent import ([`002f12c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/002f12c8))
- **Watch daemon stale detection**: Monitors for stuck indexing states with configurable thresholds and recovery actions ([`320b8bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/320b8bdf))
- **Cloudflare Pages direct API upload**: Deploy without wrangler CLI using Blake3 hashing and MIME detection ([`7776fe8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7776fe86))
- **LazyDb for startup performance**: Deferred SQLite connection with RAII guard pattern; health command and TUI startup optimized ([`03e17b4`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/03e17b49))
- **Two-tier progressive search**: Fast lexical results immediately, semantic results merge in as they complete ([`653836f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/653836fb))
- **Daemon module**: Unix domain socket-based warm model daemon with resource monitoring and graceful shutdown ([`28a094b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/28a094b3))
- **Embedder and reranker registries**: Model bake-off framework for comparing embedding and reranking models ([`809ba65`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/809ba658), [`34a3545`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/34a3545c))
- **HTML export redesign**: Message grouping, tool badge popovers, search highlighting, Terminal Noir theme, typography upgrades ([`aee1701`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/aee17014)...[`86966bb`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/86966bb4))
- **Doctor FTS5 detection**: `cass doctor --fix` detects and recreates missing FTS5 search table ([`6b1541f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6b1541fe), [#17](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/17))

### Fixed

- **Windows compatibility**: Daemon module gated behind `#[cfg(unix)]` ([`3f51c76`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3f51c764))
- **Rust stable toolchain**: Switched from nightly to stable ([`5983515`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/59835155))
- **u32 truncation**: Use `try_from` for safe integer casts ([`743702a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/743702ac))
- **Deterministic search sort**: `total_cmp` with tie-break by index for reproducible results ([`7d92b53`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7d92b53f))
- **Bakeoff division by zero**: Prevent division by zero in latency calculations ([`df836fe`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/df836fed))
- **SQL LIKE escaping**: Safe escaping and integer casts in SQL queries ([`32e0e70`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/32e0e704))
- **Socket path sanitization**: Sanitize Unix socket paths in daemon module ([`81a055b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/81a055ba))

---

## [v0.1.63] - 2026-01-27 (tag only, no GitHub Release)

[Compare v0.1.62...v0.1.63](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.62...v0.1.63)

### Added

- **HNSW approximate nearest-neighbor search**: O(log n) semantic search with configurable M/ef parameters and ~95-99% recall ([`b43cbb6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b43cbb6c))
- **`export-html` command**: Export conversations as self-contained HTML files with optional AES-256-GCM encryption (Argon2id KDF) ([`e3857e6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e3857e65))
- **Encrypted GitHub Pages web export**: Complete encrypted static site with browser-side decryption, service worker, FTS5 search via sqlite-wasm, and deployment wizard
- **Multi-machine remote sources**: `cass sources setup` wizard with SSH host discovery, probing, remote installation, rsync sync engine with SFTP fallback, provenance tracking, and path mappings
- **Factory (Droid) connector**: Full support for Factory AI's Droid coding agent
- **Comprehensive security hardening**: Path traversal protection, XSS prevention in FTS5 snippets, URL encoding bypass tests, secret detection pre-publish scanner

### Changed

- **Robot field filtering**: `--fields minimal` preset is 30-50% faster for robot mode
- **TOON output format**: Token-efficient output format for AI agent communication
- **Timing breakdown**: Robot output includes `open_ms`, `query_ms`, and phase-specific timings

---

## [v0.1.62] - 2026-01-27 (tag only)

[Compare v0.1.61...v0.1.62](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.61...v0.1.62)

Intermediate release with ANN search and export infrastructure landing. Merged into v0.1.63 above.

---

## [v0.1.56] - 2026-01-15 (tag only)

[Compare v0.1.55...v0.1.56](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.55...v0.1.56)

### Added

- **Pages export foundation**: Bundle verification CLI, pre-publish summary, share profiles (public/team/private), package manager notifications

### Fixed

- **rusqlite 0.38 compatibility**: Resolved type inference errors with new rusqlite version
- **Migration safety**: `PRAGMA foreign_keys` moved outside transaction for correct behavior
- **base64 engine API**: Pinned to >=0.21 for stable Engine API

---

## [v0.1.55] - 2026-01-06 (tag only)

[Compare v0.1.54...v0.1.55](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.54...v0.1.55)

### Added

- **Atomic file operations**: Crash-safe persistence for config and state files ([`32f83bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/32f83bd8))
- **Index rebuild from SQLite**: Rebuild Tantivy index from the SQLite database when the index is corrupted ([`dfbddc2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/dfbddc22))
- **Doctor command documentation**: README expanded with doctor command and sources setup wizard docs

---

## [v0.1.51] - 2026-01-05 (tag only)

[Compare v0.1.50...v0.1.51](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.50...v0.1.51)

### Added

- **SFTP sync fallback**: When rsync is unavailable, SFTP-based sync kicks in automatically ([`08fda20`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/08fda202))
- **SHA256 checksum verification in installer**: Pre-built binaries are now verified after download ([`58aabb0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/58aabb05))
- **JUnit XML test reports**: `cargo-nextest` integration for CI test reporting ([`8787819`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8787819c))
- **Test coverage infrastructure**: `cargo-llvm-cov` integration for code coverage ([`2335e60`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2335e605))
- **Semantic search benchmarks**: Performance benchmarks for embedding-based search ([`810d62a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/810d62ae))

### Changed

- **dotenvy adoption**: Use `dotenvy` instead of `std::env::var` across core modules and connectors

### Fixed

- **SSH config fallback**: Match by hostname as fallback when SSH alias doesn't match ([`5093f15`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5093f15c))
- **String slicing panics**: Prevent panics in `truncate_text` with small `max_len` values ([`c873e5b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c873e5b1))
- **Repair flag**: `--repair` flag fixed for model management ([`9de607e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9de607e8))

---

## [v0.1.50] - 2026-01-04 (tag only)

[Compare v0.1.49...v0.1.50](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.49...v0.1.50)

### Added

- **Semantic search integration tests**: E2E tests for semantic search flows
- **Memory tests and benchmark infrastructure**: Benchmarks for indexing, search, and caching
- **Sources unit tests**: Comprehensive tests for setup workflow and source sync

---

## [v0.1.49] - 2026-01-03 (tag only)

[Compare v0.1.48...v0.1.49](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.48...v0.1.49)

### Added

- **Local UI metrics**: `CASS_UI_METRICS` env flag for collecting local TUI performance data ([`2dec89b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2dec89bd))
- **Indexer HUD**: Throughput sparkline in the TUI during indexing ([`a0a38da`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a0a38da1))
- **Source filter UI**: Shift+F11 popup menu and F11 keyboard shortcut for cycling source filters ([`25a057b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/25a057bc), [`128d1e9`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/128d1e97))
- **WCAG AA contrast compliance**: Hint text meets WCAG AA contrast requirements ([`084974c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/084974c9))

---

## [v0.1.48] - 2025-12-30 (tag only)

[Compare v0.1.47...v0.1.48](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.47...v0.1.48)

Rapid-fire release series (v0.1.37 through v0.1.48 all landed on 2025-12-30) fixing cross-compilation issues. Key changes across this batch:

- **ARM64 Linux cross-compilation**: vendored OpenSSL for ARM64 builds; native ARM64 runner adoption ([`de83181`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/de831810))
- **base64 0.22 migration**: Updated for API compatibility ([`3ccd419`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/3ccd4196))
- **Golden contract test version-agnosticism**: Tests no longer break on version bumps ([`27dca3d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/27dca3db))

---

## [v0.1.36] - 2025-12-17 (tag only)

[Compare v0.1.35...v0.1.36](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.35...v0.1.36)

### Added

- **Multi-machine remote sources**: Complete source management system with `cass sources add/list/remove/sync/doctor` commands, rsync-based delta sync, provenance tracking schema (V8 migration), and TUI source filter ([`a2970ae`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a2970aed)...[`2aec4c3`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2aec4c3a))
- **Semantic search infrastructure**: Embedder trait, hash embedder, canonicalization module, and WSL Cursor support ([`e28c8832`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e28c8832))
- **Roo Cline and Cursor editor support**: Extended Cline connector to detect Roo Cline variant and Cursor editor sessions ([`bf27e5d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bf27e5d3))
- **Pi-Agent watch mode**: Pi-Agent added to `ConnectorKind` for watch-mode indexing support ([`4a189b7`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4a189b7e))
- **Comprehensive test documentation**: `TESTING.md` with test categories, fixtures, and CI integration ([`f4ca26d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f4ca26d6))
- **CI coverage and artifact archiving**: Test coverage reports and artifact preservation ([`db21249`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/db212494))

### Fixed

- **DST ambiguity**: Handle Daylight Saving Time ambiguity and gaps in date parsing ([`cf3a8f2`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cf3a8f2e))
- **Path traversal protection**: Prevent Unicode normalization attacks, RTL override characters, zero-width characters, and homoglyph confusables ([`25ce09d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/25ce09da))
- **Markdown injection**: Prevent injection in exported results ([`8832e92`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8832e926))
- **Phrase query semantics**: Correct phrase queries and improve tokenization ([`c105489`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/c1054891))
- **Timeline SQL bug**: Fixed SQL in timeline query and rewrote OpenCode connector for JSON storage ([`e151f26`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e151f269))

---

## [v0.1.35] - 2025-12-02 (tag only)

[Compare v0.1.34...v0.1.35](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.34...v0.1.35)

### Added

- **Pi-Agent connector**: Support for pi-mono coding-agent sessions from nested subdirectories with author/model tracking ([`b333597`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b3335970))

---

## [v0.1.34] - 2025-12-02 (tag only)

[Compare v0.1.33...v0.1.34](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.33...v0.1.34)

### Added

- **Multi-platform CI/CD**: Cross-platform release builds and self-update installer ([`2371`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/23714de5))
- **Comprehensive undocumented feature documentation**: Query language, keyboard shortcuts, ranking internals ([`6958ad5`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/6958ad5a))

---

## [v0.1.32] - 2025-12-02 (tag only)

[Compare v0.1.31...v0.1.32](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.31...v0.1.32)

### Added

- **Cursor IDE and ChatGPT desktop connectors**: Full parsing support for Cursor IDE and ChatGPT desktop app sessions ([`546c054`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/546c054b))
- **Aider connector**: Chat history parsing for the Aider AI pair programming tool ([`7c89f6d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7c89f6d5))
- **Search timeout and dry-run mode**: `--timeout` and `--dry-run` CLI flags for search ([`634c656`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/634c656f))
- **Sparkline progress visualization**: Sparkline widget shows indexing progress in the TUI ([`9f4b69c`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9f4b69c4))
- **Ctrl+Enter queue and Ctrl+O open-all**: Keyboard shortcuts for queuing searches and opening all results ([`4b6d910`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4b6d9101))
- **Parallel connector scanning**: Indexer scans connectors in parallel with agent discovery feedback ([`1120ab1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/1120ab19))

### Fixed

- **WCAG hint contrast**: Boost hint text contrast to meet WCAG compliance ([`ab52ec8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ab52ec82))
- **Transaction integrity**: Wrap storage operations in transactions with proper NULL handling ([`9b20566`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9b20566e))

---

## [v0.1.31] - 2025-12-01 (tag only)

[Compare v0.1.30...v0.1.31](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.30...v0.1.31)

### Added

- **Vim-style navigation**: `h`/`j`/`k`/`l` (or `Alt`+keys) for pane navigation
- **Manual refresh**: Ctrl+Shift+R triggers background re-index
- **Hidden pane indicators**: Visual arrows show when agent panes are scrolled out of view
- **Agent filter autocomplete**: F3 shows dropdown with matching agent names
- **Line number navigation**: Search results track exact line numbers for editor jumps (F8)
- **Time chips**: Human-readable dates in filter chips
- **Reset state**: Ctrl+Shift+Del resets TUI state to defaults

### Fixed

- **Binary name references**: Error messages now correctly reference `cass` instead of `coding-agent-search`
- **Unsafe transmute removal**: Removed unsafe code in UI rendering

---

## [v0.1.28] - 2025-11-30 (tag only)

[Compare v0.1.27...v0.1.28](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.27...v0.1.28)

### Added

- **Bookmarks and export**: Bookmark conversations and export search results ([`57127ac`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/57127aca))
- **Detail pane and inline search**: Major TUI expansion with rich detail view and in-pane search ([`b0ffa28`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b0ffa28c))
- **Wildcard and fuzzy matching**: Enhanced query engine with prefix/suffix/infix wildcards and fuzzy search ([`f85f2a0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f85f2a0e))
- **Comprehensive theme system**: WCAG accessibility support with multiple color schemes ([`42bf621`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/42bf6218))
- **Modular UI components**: Extracted reusable UI widgets for the TUI ([`e7d4875`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e7d4875e))

---

## [v0.1.27] - 2025-11-28 (tag only)

[Compare v0.1.26...v0.1.27](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.26...v0.1.27)

### Added

- **Implicit wildcard fallback**: Sparse results automatically retry with wildcard matching
- **Explicit wildcard search**: Support for `*` prefix/suffix wildcards in queries
- **Indexing status visibility**: Real-time status updates during indexing

---

## [v0.1.26] - 2025-11-27 (tag only)

[Compare v0.1.25...v0.1.26](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.25...v0.1.26)

### Changed

- **Premium theme system**: Complete UI overhaul with Stripe-level aesthetics ([`4e6058e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4e6058e5))

---

## [v0.1.25] - 2025-11-27 (tag only)

[Compare v0.1.24...v0.1.25](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.24...v0.1.25)

### Fixed

- **Connector message index consistency**: Fixed message index assignment in Claude Code, Codex, and Gemini connectors ([`04ed880`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/04ed8809))
- **Multibyte UTF-8 snippet truncation**: Fixed crash when truncating snippets at multi-byte character boundaries ([`cf26dcc`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cf26dccd))

---

## [v0.1.24] - 2025-11-27 (tag only)

[Compare v0.1.23...v0.1.24](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.23...v0.1.24)

### Added

- **Read-only database access for TUI**: Detail view loads data from SQLite without write locks ([`7e9118b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/7e9118b2))
- **Incremental connector filtering**: Connectors properly filter by `since_ts` for incremental indexing ([`27e0ef8`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/27e0ef88))

### Changed

- **Tantivy batch commits**: Commit immediately after each connector batch in watch mode ([`47f5a0f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/47f5a0f3))
- **Search bar wrapping disabled**: Text wrapping disabled for cursor visibility ([`ff80172`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/ff801727))

---

## [v0.1.23] - 2025-11-27 (tag only)

[Compare v0.1.22...v0.1.23](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.22...v0.1.23)

### Added

- **Search schema v4**: Edge n-gram prefix fields and preview for type-ahead search ([`f77fc0e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f77fc0e4))
- **LRU prefix cache and bloom filter**: Search caching for instant repeat-query responses ([`4d36852`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4d368525))
- **TUI progress display**: Real-time indexing progress, markdown rendering, adaptive footer, Unicode safety ([`4f3b336`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/4f3b336c))
- **Atomic indexer progress tracking**: Thread-safe progress reporting for TUI integration ([`5fc77ee`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5fc77ee1))

---

## [v0.1.22] - 2025-11-26 (tag only)

[Compare v0.1.21...v0.1.22](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.21...v0.1.22)

### Added

- **Search schema v2**: `created_at` field in search index; hit deduplication and query sanitization ([`5206b66`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/5206b662))
- **Robot mode automation contract**: Published contract for machine-readable CLI output ([`bf3c249`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/bf3c2491))
- **Search pagination and quiet mode**: `--offset` and `--quiet` flags for robot-mode usage ([`96e2b25`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/96e2b259))
- **TUI richer detail modal**: Improved parsing and updated hotkey/help coverage ([`448603a`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/448603a5))

---

## [v0.1.21] - 2025-11-25 (tag only)

[Compare v0.1.19...v0.1.21](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.19...v0.1.21)

### Added

- **Major UX polish (Sprint 5)**: Comprehensive TUI improvements ([`b5242f0`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b5242f0f))
- **Connector rewrite**: Connectors rewritten to properly parse real agent data formats ([`e492d1b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/e492d1b6))

---

## v0.1.15 - v0.1.19 - 2025-11-25 (tags only)

Rapid-fire patch releases fixing binary naming and build issues:

- **v0.1.19**: Fix update loop caused by version check ([`35fecaf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/35fecafe))
- **v0.1.17**: Move `main.rs` back to root; configure bin name `cass` in Cargo.toml ([`2aa5edf`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/2aa5edf1))
- **v0.1.15**: Move `main.rs` to `src/bin/cass.rs` (reverted in v0.1.17) ([`8893b03`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8893b035))
- **v0.1.13**: Fix UI artifacts in help overlay and F11 key conflict ([`a202ced`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a202ced8))

---

## [v0.1.9] - 2025-11-24 (tag only)

[Compare v0.1.5...v0.1.9](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.5...v0.1.9)

### Added

- **Global Ctrl+C handling**: Clean exit on Ctrl+C from any TUI state ([`98393aa`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/98393aaa))
- **Updated TUI keymap documentation**: README updated with complete keyboard shortcut reference

---

## [v0.1.5] - 2025-11-24 (tag only)

[Compare v0.1.4...v0.1.5](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.4...v0.1.5)

### Added

- **Zero-hit suggestions**: When no results are found, the TUI displays helpful suggestions for broadening the search
- **Improved visual feedback**: Mode indicators and responsiveness improvements ([`abdb82b`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/abdb82b7))

---

## [v0.1.4] - 2025-11-24 (tag only)

[Compare v0.1.3...v0.1.4](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.3...v0.1.4)

### Changed

- **Rename binary to `cass`**: Binary renamed from `coding-agent-search` to `cass` ([`196945e`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/196945e8))
- **Default to TUI mode**: Running `cass` without arguments launches the TUI with background indexing
- **Logs moved to file**: Application logs redirected from stderr to a log file

### Added

- **TUI chips bar**: Filter chips, ranking presets, pane density controls, peek badge, and persistent controls ([`8944d30`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8944d301))

---

## [v0.1.3] - 2025-11-24 (tag only)

[Compare v0.1.2...v0.1.3](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.2...v0.1.3)

### Fixed

- Installer artifact paths and release packaging ([`0d112f1`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/0d112f1e))

---

## [v0.1.2] - 2025-11-24 (tag only)

[Compare v0.1.1...v0.1.2](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.1...v0.1.2)

### Fixed

- Distribution workflow fixes: artifact path (`target/distrib`), dist init step, checksum debugging ([`8a5c9d6`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/8a5c9d6a)...[`d8c2323`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d8c23237))

---

## [v0.1.1] - 2025-11-24 (tag only)

[Compare v0.1.0...v0.1.1](https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.0...v0.1.1)

### Added

- **CI/CD pipeline**: Comprehensive release workflow with automated GitHub Releases ([`f5ffbce`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/f5ffbceb))
- **Cross-platform installers**: `install.sh` with easy mode, verify, quickstart, and rustup bootstrap; `install.ps1` for Windows with checksum verification ([`cfac576`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/cfac5764))
- **Build-from-source fallback**: `--from-source` flag in installer for platforms without pre-built binaries ([`88fb89d`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/88fb89d2))
- **E2E installer testing**: Comprehensive end-to-end tests for the installation process ([`735897f`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/735897f1))

### Fixed

- **FTS rebuild performance**: Fixed critical performance issue in FTS5 rebuild ([`d4fd6ab`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/d4fd6abb))
- **Gemini connector collision**: Fixed message indexing collision and ensured deterministic file order ([`349d0bd`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/349d0bd6))
- **Snippet extraction**: Use Tantivy SnippetGenerator and SQLite `snippet()` function correctly ([`a9b0241`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/a9b02411))

---

## [v0.1.0] - 2025-11-23 (tag only)

[Tag](https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.0)

Initial release. The project was built from scratch over 3 days (initial commit: 2025-11-20).

### Core Features

- **6 agent connectors**: Claude Code, Codex CLI, Gemini CLI, Cline, OpenCode, Amp -- each parsing native session formats into a normalized data model
- **Dual search backend**: Tantivy full-text index + SQLite FTS5 virtual table for redundant search coverage
- **Interactive TUI**: Three-pane layout (search bar, results list, detail view) with multi-mode filtering, pagination, and rich theming via Ratatui
- **SQLite storage layer**: Schema v1 with migrations, FTS5 optimization, and structured conversation storage
- **Watch-mode indexing**: File watcher with mtime high-water marks for incremental indexing and debounce logic
- **Editor integration**: Open results in `$EDITOR` at the correct line number
- **CLI commands**: `cass index` (full/incremental), `cass search` (with robot mode), `cass` (TUI)
- **Homebrew and Scoop formulae**: Package manager support for macOS and Windows
- **Benchmarking suite**: Runtime performance benchmarks for indexing and search
- **CI workflow**: GitHub Actions for build, test, and release

---

<!-- link references -->
[Unreleased]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.2.2...HEAD
[v0.2.2]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.2
[v0.2.1]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.1
[v0.2.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0
[v0.1.64]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.64
[v0.1.63]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.62...v0.1.63
[v0.1.62]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.61...v0.1.62
[v0.1.56]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.55...v0.1.56
[v0.1.55]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.54...v0.1.55
[v0.1.51]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.50...v0.1.51
[v0.1.50]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.49...v0.1.50
[v0.1.49]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.48...v0.1.49
[v0.1.48]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.47...v0.1.48
[v0.1.36]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.35...v0.1.36
[v0.1.35]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.34...v0.1.35
[v0.1.34]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.33...v0.1.34
[v0.1.32]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.31...v0.1.32
[v0.1.31]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.30...v0.1.31
[v0.1.28]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.27...v0.1.28
[v0.1.27]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.26...v0.1.27
[v0.1.26]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.25...v0.1.26
[v0.1.25]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.24...v0.1.25
[v0.1.24]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.23...v0.1.24
[v0.1.23]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.22...v0.1.23
[v0.1.22]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.21...v0.1.22
[v0.1.21]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.19...v0.1.21
[v0.1.9]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.5...v0.1.9
[v0.1.5]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.4...v0.1.5
[v0.1.4]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.3...v0.1.4
[v0.1.3]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.2...v0.1.3
[v0.1.2]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.1...v0.1.2
[v0.1.1]: https://github.com/Dicklesworthstone/coding_agent_session_search/compare/v0.1.0...v0.1.1
[v0.1.0]: https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.1.0
